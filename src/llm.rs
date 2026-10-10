use crate::config::Config;
use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    error::Error as StdError,
    fmt::{Display, Formatter},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod dns;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
pub const MAX_TOOL_CALL_ID_BYTES: usize = 256;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
pub const MAX_COMPLETION_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOOL_CALLS: usize = 32;

pub fn validate_completion_bounds(completion: &Completion) -> Result<()> {
    if completion.calls.len() > MAX_TOOL_CALLS {
        bail!("tool_call_batch_limit: at most {MAX_TOOL_CALLS} calls");
    }
    let mut bytes = completion.text.len();
    for call in &completion.calls {
        if call.id.len() > MAX_TOOL_CALL_ID_BYTES || call.name.len() > MAX_TOOL_NAME_BYTES {
            bail!("malformed_tool_call: call ID or name is too long");
        }
        bytes = bytes
            .saturating_add(call.id.len())
            .saturating_add(call.name.len())
            .saturating_add(call.arguments.len());
    }
    if bytes > MAX_COMPLETION_BYTES {
        bail!("response_size_limit");
    }
    Ok(())
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: usize,
    pub output: usize,
    pub cached: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptAction {
    Retry,
    JsonModeFallback,
    UnstructuredFallback,
    Stop,
}

/// A failed provider attempt, retained even when a later attempt succeeds.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptDiagnostic {
    pub attempt: usize,
    pub code: String,
    pub reason: String,
    pub action: AttemptAction,
}

/// An attempt the provider refused with an HTTP status never started
/// generating, so it is not billed and needs no usage estimate. In-stream
/// failures, timeouts and invalid output may have consumed tokens.
fn unbilled_attempts(diagnostics: &[AttemptDiagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|attempt| attempt.code.starts_with("http_"))
        .count()
}

#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub attempts: usize,
    pub attempt_diagnostics: Vec<AttemptDiagnostic>,
    pub length_limited: bool,
    pub discarded_tool_calls: bool,
    /// The upstream provider a gateway such as OpenRouter routed the request
    /// to, when its stream names one.
    pub provider: Option<String>,
    /// Seconds from sending the request to its first stream event. The same
    /// request took 116 s in one run and 11 s in another; this tells waiting
    /// at the provider from generating.
    pub first_event_seconds: Option<f64>,
}

impl Completion {
    /// Failed attempts before this successful one that may have been billed.
    pub fn billable_failed_attempts(&self) -> usize {
        self.attempts
            .saturating_sub(1)
            .saturating_sub(unbilled_attempts(&self.attempt_diagnostics))
    }
}

/// The provider may retry a request several times before returning an error.
/// Keep the attempt count for budgeting and diagnostics for successful retries
/// as well as terminal failures. Display remains the terminal error message.
#[derive(Debug)]
pub struct CompletionError {
    source: anyhow::Error,
    attempts: usize,
    provider_unavailable: bool,
    attempt_diagnostics: Vec<AttemptDiagnostic>,
}
impl CompletionError {
    pub(crate) fn new(source: anyhow::Error, attempts: usize, provider_unavailable: bool) -> Self {
        Self {
            source,
            attempts,
            provider_unavailable,
            attempt_diagnostics: Vec::new(),
        }
    }

    pub fn attempts(&self) -> usize {
        self.attempts.max(1)
    }

    /// Attempts that may have been billed. Without diagnostics every attempt
    /// counts, so the estimate stays conservative.
    pub fn billable_attempts(&self) -> usize {
        self.attempts()
            .saturating_sub(unbilled_attempts(&self.attempt_diagnostics))
    }

    pub fn attempt_diagnostics(&self) -> &[AttemptDiagnostic] {
        &self.attempt_diagnostics
    }

    pub(crate) fn with_diagnostics(mut self, diagnostics: Vec<AttemptDiagnostic>) -> Self {
        self.attempt_diagnostics = diagnostics;
        self
    }

    /// The provider stayed unavailable (overload, 5xx, dropped stream) through
    /// every quick retry and no text reached the user, so the same request
    /// may be sent again after a longer wait.
    pub(crate) fn provider_unavailable(&self) -> bool {
        self.provider_unavailable
    }
}

/// A request that got no answer in time: no stream data within the request
/// timeout, a gateway timeout or an upstream idle timeout.
pub(crate) fn timeout_error(text: &str) -> bool {
    text.starts_with("request_timeout")
        || text.starts_with("http_504")
        || text.contains("\"code\":504")
        || text.to_ascii_lowercase().contains("idle timeout")
}

/// An in-stream error from an overloaded or rate-limited provider. Like a
/// timeout, it says nothing about the response format.
fn overload_error(text: &str) -> bool {
    text.contains("\"code\":429")
        || text.contains("\"code\":503")
        || text.contains("\"error_type\":\"provider_overloaded\"")
}

/// Failures of the provider or the connection, not of the request itself.
fn transient_error(text: &str) -> bool {
    text.starts_with("provider_stream_error:")
        || text.starts_with("stream_interrupted:")
        || text.starts_with("request_timeout")
        || text.starts_with("http_429")
        || text.starts_with("http_5")
        || text.contains("error sending request")
}
impl Display for CompletionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}
impl StdError for CompletionError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> Result<Completion>;
}
#[derive(Default)]
pub struct OpenAiClient;
pub(crate) const STREAM_DELTAS_MARKER: &str = "__mnemoarc_stream_deltas";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 512;

fn record_attempt_failure(
    diagnostics: &mut Vec<AttemptDiagnostic>,
    request_id: &str,
    attempt: usize,
    error: &str,
    action: AttemptAction,
    c: &Config,
) {
    // Providers sometimes echo credentials in an error body. Redact before
    // clipping so a prefix of a long key cannot leak through truncation.
    let redacted = if let Some(key) = OpenAiClient::key(c) {
        error.replace(&key, "[redacted]")
    } else {
        error.to_owned()
    };
    let prefix = redacted.split(':').next().unwrap_or("");
    let code = if !prefix.is_empty()
        && prefix.len() <= 64
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        prefix.to_owned()
    } else {
        "request_error".into()
    };
    let reason = redacted.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    let diagnostic = AttemptDiagnostic {
        attempt,
        code,
        reason,
        action,
    };
    // Group interleaved attempts from concurrent requests without logging the
    // request payload. JSON also keeps provider newlines on one log line.
    crate::console::notice(
        crate::console::Target::Stderr,
        format!(
            "[llm] {}\n",
            json!({"request_id":request_id,"attempt_failure":diagnostic})
        ),
    );
    diagnostics.push(diagnostic);
}

fn apply_thinking_settings(request: &mut Value, c: &Config) {
    if c.enable_thinking {
        if let Some(effort) = &c.reasoning_effort {
            request["reasoning_effort"] = json!(effort);
        }
    } else {
        // OpenAI-compatible providers, including OpenRouter/Qwen, map the
        // standard `none` effort to the model's non-thinking mode. This must
        // override a stale effort value kept in settings for re-enabling.
        request["reasoning_effort"] = json!("none");
    }
}

fn response_format_rejected(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    // OpenAI-compatible servers do not agree on the error body: some name
    // response_format, while others return only a generic parameter 400/422.
    error.starts_with("http_400:")
        || error.starts_with("http_422:")
        || (error.starts_with("http_4")
            && (error.contains("response_format")
                || error.contains("json_object")
                || error.contains("unsupported parameter")))
}

/// Endpoints (base URL and model) that rejected strict JSON Schema output.
/// Later requests use plain JSON mode directly instead of paying a failed
/// attempt each time.
static JSON_SCHEMA_REJECTED: std::sync::Mutex<SchemaCache> =
    std::sync::Mutex::new(SchemaCache(VecDeque::new()));

/// Some providers fail a particular strict grammar inside an otherwise valid
/// SSE response. Remember only a successful JSON-mode recovery, per schema;
/// a provider outage alone must not disable all structured output.
static JSON_SCHEMA_STREAM_RECOVERED: std::sync::Mutex<SchemaCache> =
    std::sync::Mutex::new(SchemaCache(VecDeque::new()));

/// Schemas recovered by JSON mode once. An outage that clears before the
/// comparison attempt looks the same as a grammar failure (a live run lost
/// strict output for a whole run after three in-stream 502s), so a schema
/// moves to JSON_SCHEMA_STREAM_RECOVERED only when a second request needs the
/// same recovery; a strict response that succeeds clears the suspicion.
static JSON_SCHEMA_STREAM_SUSPECTED: std::sync::Mutex<SchemaCache> =
    std::sync::Mutex::new(SchemaCache(VecDeque::new()));

/// Endpoints (base URL and model) that rejected every response_format,
/// strict schema and plain JSON mode alike. Recorded only after the same
/// request then succeeded without one, so an unrelated 400 cannot disable
/// JSON mode. A live run otherwise paid a failed attempt on every structured call.
static RESPONSE_FORMAT_REJECTED: std::sync::Mutex<SchemaCache> =
    std::sync::Mutex::new(SchemaCache(VecDeque::new()));

struct SchemaCache(VecDeque<[u8; 32]>);
impl SchemaCache {
    const MAX_ENTRIES: usize = 128;

    fn contains(&mut self, key: &[u8; 32]) -> bool {
        let Some(index) = self.0.iter().position(|known| known == key) else {
            return false;
        };
        let key = self.0.remove(index).unwrap();
        self.0.push_back(key);
        true
    }

    fn remove(&mut self, key: &[u8; 32]) -> bool {
        let Some(index) = self.0.iter().position(|known| known == key) else {
            return false;
        };
        self.0.remove(index);
        true
    }

    fn insert(&mut self, key: [u8; 32]) {
        if self.contains(&key) {
            return;
        }
        if self.0.len() == Self::MAX_ENTRIES {
            self.0.pop_front();
        }
        self.0.push_back(key);
    }
}

fn schema_endpoint(c: &Config) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in [&c.base_url, &c.model] {
        hash.update(part.len().to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

fn schema_cache_key(c: &Config, format: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(schema_endpoint(c));
    hash.update(format.to_string().as_bytes());
    hash.finalize().into()
}

/// Record how a strict-schema request finished. A JSON-mode recovery after
/// the SSE fallback caches the schema only when the same schema already
/// needed it on an earlier request; a strict success clears that suspicion.
fn note_schema_stream_result(key: &[u8; 32], request: &Value, stream_fallback: bool) {
    let Ok(mut suspected) = JSON_SCHEMA_STREAM_SUSPECTED.lock() else {
        return;
    };
    match request["response_format"]["type"].as_str() {
        Some("json_schema") => {
            suspected.remove(key);
        }
        Some("json_object") if stream_fallback => {
            if suspected.remove(key) {
                if let Ok(mut recovered) = JSON_SCHEMA_STREAM_RECOVERED.lock() {
                    recovered.insert(*key);
                }
            } else {
                suspected.insert(*key);
            }
        }
        _ => {}
    }
}

fn downgrade_json_schema(request: &mut Value) -> bool {
    if request["response_format"]["type"] != "json_schema" {
        return false;
    }
    request["response_format"] = json!({"type":"json_object"});
    true
}

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: String,
    event_bytes: usize,
    skip_lf: bool,
    started: bool,
    has_data: bool,
}
impl SseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut result = vec![];
        for &byte in bytes {
            if let Some(event) = self.feed_byte(byte)? {
                result.push(event);
            }
        }
        Ok(result)
    }

    // Deliver each event before decoding later bytes so a consumer can stop
    // at its terminator, even when the rest of that HTTP chunk is malformed.
    fn feed_byte(&mut self, byte: u8) -> Result<Option<String>> {
        // SSE permits CR, LF and CRLF, including a split CRLF.
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        self.event_bytes += 1;
        if self.event_bytes > MAX_COMPLETION_BYTES {
            bail!("response_size_limit: SSE event too large");
        }
        if matches!(byte, b'\r' | b'\n') {
            let event = self.finish_line()?;
            self.skip_lf = byte == b'\r';
            Ok(event)
        } else {
            self.line.push(byte);
            Ok(None)
        }
    }

    fn finish_line(&mut self) -> Result<Option<String>> {
        let mut line = std::str::from_utf8(&self.line)?;
        if !self.started {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.started = true;
        }
        let mut event = None;
        if line.is_empty() {
            if self.has_data {
                self.data.pop(); // Remove only the final data-line delimiter.
                event = Some(std::mem::take(&mut self.data));
                self.has_data = false;
            }
            self.event_bytes = 0;
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                self.data.push('\n');
                self.has_data = true;
            }
        }
        self.line.clear();
        Ok(event)
    }
}
impl OpenAiClient {
    fn client(c: &Config) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .dns_resolver(Arc::new(dns::SystemDns::default()));
        if c.disable_proxy {
            b = b.no_proxy();
        } else if let Some(proxy) = &c.proxy {
            b = b.proxy(reqwest::Proxy::all(proxy)?)
        }
        Ok(b.build()?)
    }
    pub fn has_key(c: &Config) -> bool {
        Self::key(c).is_some()
    }
    fn key(c: &Config) -> Option<String> {
        c.api_key
            .as_ref()
            .map(|s| s.0.clone())
            .or_else(|| std::env::var(&c.api_key_env).ok())
            .or_else(|| {
                crate::config::dotenv_entries()?
                    .filter_map(Result::ok)
                    .find(|(k, _)| k == &c.api_key_env)
                    .map(|(_, v)| v)
            })
            .filter(|value| !value.trim().is_empty())
    }
    async fn attempt(
        &self,
        mut request: Value,
        c: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
        emitted_text: Arc<AtomicBool>,
        stream_deltas: bool,
    ) -> Result<Completion> {
        request["stream"] = json!(true);
        request[if c.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        }] = json!(c.output_tokens);
        if c.stream_usage {
            request["stream_options"] = json!({"include_usage":true});
        }
        apply_thinking_settings(&mut request, c);
        let mut req = Self::client(c)?.post(c.completion_url()?).json(&request);
        if let Some(key) = Self::key(c) {
            req = req.bearer_auth(key)
        }
        // request_timeout_secs bounds waiting for response headers and each
        // silent gap in the stream, not the whole response: a long reasoning
        // answer that keeps streaming (or sends keepalives) is not cut off.
        // The run deadline bounds total duration.
        let idle = Duration::from_secs(c.request_timeout_secs);
        let sent = std::time::Instant::now();
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("cancelled"),
            r = tokio::time::timeout(idle, req.send()) => r.map_err(|_| {
                anyhow::anyhow!("request_timeout: no response within {}s", c.request_timeout_secs)
            })??,
        };
        if !response.status().is_success() {
            let status = response.status();
            // Error responses can have a slow or unbounded body too. Keep
            // cancellation responsive while collecting a bounded diagnostic.
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while bytes.len() < MAX_ERROR_BODY_BYTES {
                let next = tokio::select! {
                    _ = cancel.cancelled() => bail!("cancelled"),
                    chunk = tokio::time::timeout(idle, stream.next()) => chunk,
                };
                let Ok(Some(chunk)) = next else {
                    break;
                };
                let Ok(chunk) = chunk else { break };
                let remaining = MAX_ERROR_BODY_BYTES - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            let body = String::from_utf8_lossy(&bytes).into_owned();
            bail!(
                "http_{}: {}",
                status.as_u16(),
                body.chars().take(1000).collect::<String>()
            );
        }
        let mut stream = response.bytes_stream();
        let mut parser = SseDecoder::default();
        let mut out = Completion::default();
        let mut calls: BTreeMap<usize, ToolCall> = BTreeMap::new();
        let mut done = false;
        let mut finish_reason: Option<String> = None;
        let mut expects_calls = false;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => bail!("cancelled"),
                chunk = tokio::time::timeout(idle, stream.next()) => chunk.map_err(|_| {
                    anyhow::anyhow!("request_timeout: no stream data for {}s", c.request_timeout_secs)
                })?,
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| anyhow::anyhow!("stream_interrupted: {e}"))?;
            for &byte in chunk.iter() {
                let Some(event) = parser.feed_byte(byte)? else {
                    continue;
                };
                if event.is_empty() {
                    continue;
                }
                if event == "[DONE]" {
                    done = true;
                    break;
                }
                let v: Value = serde_json::from_str(&event)
                    .map_err(|e| anyhow::anyhow!("invalid_stream_event: {e}"))?;
                if out.first_event_seconds.is_none() {
                    out.first_event_seconds = Some(sent.elapsed().as_secs_f64());
                }
                if out.provider.is_none() {
                    out.provider = v["provider"].as_str().map(str::to_owned);
                }
                if !v["error"].is_null() {
                    // Gateways such as OpenRouter open the stream with 200 and
                    // report an upstream overload or rate limit as an error
                    // event. Classify it like the matching HTTP status so the
                    // transient retry applies; other errors stay terminal.
                    let code = v["error"]["code"]
                        .as_u64()
                        .or_else(|| v["error"]["code"].as_str().and_then(|c| c.parse().ok()));
                    if code.is_some_and(|code| code == 429 || (500..600).contains(&code)) {
                        bail!("provider_stream_error: {}", v["error"]);
                    }
                    bail!("provider_error: {}", v["error"]);
                }
                if let Some(u) = v.get("usage").filter(|u| !u.is_null())
                    && let (Some(input), Some(output)) =
                        (u["prompt_tokens"].as_u64(), u["completion_tokens"].as_u64())
                {
                    out.usage = Some(Usage {
                        input: input as usize,
                        output: output as usize,
                        cached: u["prompt_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .map(|n| n as usize),
                    });
                }
                let Some(choice) = v["choices"].as_array().and_then(|a| a.first()) else {
                    continue;
                };
                if finish_reason.is_some()
                    && (choice["delta"]["content"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                        || choice["delta"]["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty()))
                {
                    bail!("incomplete_completion: delta after finish reason");
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    if let Some(previous) = finish_reason.as_deref() {
                        if previous != reason {
                            bail!(
                                "incomplete_completion: multiple finish reasons ({previous}, {reason})"
                            );
                        }
                    } else {
                        if !["stop", "tool_calls", "length"].contains(&reason) {
                            if reason == "error" {
                                bail!("provider_stream_error: finish_reason=error");
                            }
                            bail!("incomplete_completion: {reason}");
                        }
                        out.length_limited = reason == "length";
                        expects_calls = reason == "tool_calls";
                        finish_reason = Some(reason.to_owned());
                    }
                }
                if let Some(text) = choice["delta"]["content"].as_str() {
                    out.text.push_str(text);
                    if stream_deltas
                        && delta.send(text.to_string()).await.is_ok()
                        && !text.is_empty()
                    {
                        emitted_text.store(true, Ordering::Relaxed);
                    }
                }
                if let Some(entries) = choice["delta"]["tool_calls"].as_array() {
                    for call in entries {
                        let index = call["index"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("tool call missing index"))?
                            as usize;
                        let target = calls.entry(index).or_insert(ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        if let Some(id) = call["id"].as_str() {
                            target.id = id.into();
                        }
                        if let Some(name) = call["function"]["name"].as_str() {
                            target.name.push_str(name);
                        }
                        if let Some(args) = call["function"]["arguments"].as_str() {
                            target.arguments.push_str(args);
                        }
                        if target.id.len() > MAX_TOOL_CALL_ID_BYTES
                            || target.name.len() > MAX_TOOL_NAME_BYTES
                        {
                            bail!("malformed_tool_call: call ID or name is too long");
                        }
                    }
                }
                if calls.len() > MAX_TOOL_CALLS {
                    bail!("tool_call_batch_limit: at most {MAX_TOOL_CALLS} calls");
                }
                let bytes = calls.values().fold(out.text.len(), |bytes, call| {
                    bytes
                        .saturating_add(call.id.len())
                        .saturating_add(call.name.len())
                        .saturating_add(call.arguments.len())
                });
                if bytes > MAX_COMPLETION_BYTES {
                    bail!("response_size_limit");
                }
            }
            if done {
                break;
            }
        }
        if !done || finish_reason.is_none() {
            bail!("stream_interrupted: missing completion terminator");
        }
        if out.length_limited {
            // A length-limited batch may contain syntactically valid but unfinished
            // instructions. Never expose any of its calls for execution.
            out.discarded_tool_calls = !calls.is_empty();
            return Ok(out);
        }
        if expects_calls && calls.is_empty() {
            bail!("incomplete_completion: tool_calls finish without a call");
        }
        // Some OpenAI-compatible providers finish a complete tool-call
        // response with `stop` instead of `tool_calls`. The calls are still
        // checked for IDs, names and complete JSON objects below before the
        // agent can use them.
        let mut ids = std::collections::BTreeSet::new();
        for call in calls.values_mut() {
            // Providers send "" for a call without arguments; that is an
            // empty object, so the executor can name any required field.
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".into();
            }
            if call.id.is_empty() || call.name.is_empty() {
                bail!(
                    "malformed_tool_call: a tool call has an empty {}; every call needs a unique ID and an exact tool name",
                    if call.id.is_empty() {
                        "ID"
                    } else {
                        "tool name"
                    }
                );
            }
            if !ids.insert(call.id.clone()) {
                bail!(
                    "malformed_tool_call: call ID {:?} is used by more than one call in this response; call IDs must be unique",
                    call.id
                );
            }
            let _: serde_json::Map<String, Value> =
                serde_json::from_str(&call.arguments).map_err(|e| {
                    anyhow::anyhow!(
                        "invalid_tool_arguments: {} arguments are not one complete JSON object ({e}); use double-quoted property names and close every string, array and object",
                        call.name
                    )
                })?;
        }
        out.calls = calls.into_values().collect();
        validate_completion_bounds(&out)?;
        Ok(out)
    }
    pub async fn probe(&self, c: &Config) -> Result<String> {
        c.runnable()?;
        // The request timeout measures silence. A server that keeps sending
        // SSE comments can otherwise keep a connection check alive forever.
        let timeout_secs = c
            .run_timeout_secs
            .min(c.request_timeout_secs.saturating_mul(3));
        tokio::time::timeout(Duration::from_secs(timeout_secs), self.probe_roundtrip(c))
            .await
            .map_err(|_| {
                anyhow::anyhow!("connection_probe_timeout: check exceeded {timeout_secs}s")
            })?
    }
    async fn probe_roundtrip(&self, c: &Config) -> Result<String> {
        let mut body = json!({"model":c.model,"messages":[{"role":"user","content":"Reply OK"}],"stream":false});
        body[if c.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        }] = json!(c.output_tokens);
        apply_thinking_settings(&mut body, c);
        let mut req = Self::client(c)?.post(c.completion_url()?).json(&body);
        if let Some(k) = Self::key(c) {
            req = req.bearer_auth(k)
        }
        let plain =
            tokio::time::timeout(Duration::from_secs(c.request_timeout_secs), req.send()).await??;
        if !plain.status().is_success() {
            bail!("plain response probe failed: {}", plain.status());
        }
        // HTTP 200 alone can be an HTML proxy page or a provider error. Read
        // and validate the plain answer before testing streaming/tool support.
        let mut stream = plain.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) =
            tokio::time::timeout(Duration::from_secs(c.request_timeout_secs), stream.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("plain response probe failed: response body timeout")
                })?
        {
            let chunk =
                chunk.map_err(|error| anyhow::anyhow!("plain response probe failed: {error}"))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_COMPLETION_BYTES {
                bail!("plain response probe failed: response_size_limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let plain: Value = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!("plain response probe failed: invalid JSON: {error}")
        })?;
        let choice = &plain["choices"][0];
        if !plain["error"].is_null()
            || choice["message"]["content"]
                .as_str()
                .is_none_or(|text| text.trim().is_empty())
            || choice["message"]["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            || choice["finish_reason"].as_str() != Some("stop")
        {
            bail!("plain response probe failed: expected a complete text response");
        }
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let tool = json!({"type":"function","function":{"name":"connection_echo","description":"Return the supplied text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}});
        let mut request = json!({"model":c.model,"messages":[{"role":"user","content":"Call connection_echo with text OK"}],"tools":[tool],"tool_choice":{"type":"function","function":{"name":"connection_echo"}}});
        let completion = self
            .complete(request.clone(), c, CancellationToken::new(), tx.clone())
            .await?;
        if completion.calls.len() != 1 || completion.calls[0].name != "connection_echo" {
            bail!("tool probe did not return expected call");
        }
        let arguments: Value = serde_json::from_str(&completion.calls[0].arguments)?;
        if arguments["text"].as_str() != Some("OK") {
            bail!("tool probe did not return the requested text argument");
        }
        request.as_object_mut().unwrap().remove("tool_choice");
        let call = &completion.calls[0];
        request["messages"].as_array_mut().unwrap().extend([json!({"role":"assistant","content":null,"tool_calls":[{"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}),json!({"role":"tool","tool_call_id":call.id,"content":"OK"})]);
        let final_response = self
            .complete(request, c, CancellationToken::new(), tx)
            .await?;
        if final_response.length_limited {
            bail!("incomplete_completion: length during connection probe");
        }
        if !final_response.calls.is_empty() || final_response.text.trim().is_empty() {
            bail!("tool round-trip probe failed: expected a complete text response without tools");
        }
        Ok("Plain response, SSE streaming and tool round-trip succeeded".into())
    }
}
#[async_trait]
impl LlmClient for OpenAiClient {
    async fn complete(
        &self,
        mut request: Value,
        c: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> Result<Completion> {
        // The client is also called directly (outside the agent's guards).
        c.validate()?;
        if !request.is_object() {
            bail!("invalid_request: completion request must be a JSON object");
        }
        let stream_deltas = request
            .get(STREAM_DELTAS_MARKER)
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if let Some(fields) = request.as_object_mut() {
            fields.remove(STREAM_DELTAS_MARKER);
        }
        let schema_key = (request["response_format"]["type"] == "json_schema")
            .then(|| schema_cache_key(c, &request["response_format"]));
        if JSON_SCHEMA_REJECTED
            .lock()
            .is_ok_and(|mut rejected| rejected.contains(&schema_endpoint(c)))
            || schema_key.as_ref().is_some_and(|key| {
                JSON_SCHEMA_STREAM_RECOVERED
                    .lock()
                    .is_ok_and(|mut recovered| recovered.contains(key))
            })
        {
            downgrade_json_schema(&mut request);
        }
        let format_rejected = request.get("response_format").is_some()
            && RESPONSE_FORMAT_REJECTED
                .lock()
                .is_ok_and(|mut rejected| rejected.contains(&schema_endpoint(c)));
        if format_rejected && let Some(fields) = request.as_object_mut() {
            fields.remove("response_format");
        }
        let emitted_text = Arc::new(AtomicBool::new(false));
        let mut attempt = 0usize;
        // The optional response-format fallback is a compatibility attempt,
        // not one of the configured transient retries. Keep its attempt in
        // the usage accounting, but track retry budget separately so a
        // fallback cannot either consume all retries or make retries=0 loop
        // forever after a subsequent 429/5xx response.
        let mut transient_retries = 0usize;
        let mut response_format_fallback = format_rejected;
        let mut schema_stream_fallback = false;
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut attempt_diagnostics = Vec::new();
        loop {
            // Cancellation must cover every await inside an attempt, including
            // response-body reads and backpressure on the delta channel.
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("cancelled"),
                result = self.attempt(
                    request.clone(),
                    c,
                    cancel.clone(),
                    delta.clone(),
                    emitted_text.clone(),
                    stream_deltas,
                ) => result,
            };
            match result {
                Ok(mut r) => {
                    if let Some(key) = &schema_key {
                        note_schema_stream_result(key, &request, schema_stream_fallback);
                    }
                    // This request recovered by dropping a rejected format.
                    if response_format_fallback
                        && !format_rejected
                        && let Ok(mut rejected) = RESPONSE_FORMAT_REJECTED.lock()
                    {
                        rejected.insert(schema_endpoint(c));
                    }
                    r.attempts = attempt.saturating_add(1);
                    r.attempt_diagnostics = attempt_diagnostics;
                    return Ok(r);
                }
                Err(e) => {
                    let text = e.to_string();
                    // OpenAI-compatible local servers are not uniform about
                    // JSON mode. Retry once without the optional structured
                    // output hint when the server explicitly rejects it;
                    // finish() still validates the returned object.
                    // Strict schemas degrade to plain JSON mode first, then to
                    // no response format; each step is one compatibility attempt.
                    if request.get("response_format").is_some()
                        && response_format_rejected(&text)
                        && downgrade_json_schema(&mut request)
                    {
                        if let Ok(mut rejected) = JSON_SCHEMA_REJECTED.lock() {
                            rejected.insert(schema_endpoint(c));
                        }
                        record_attempt_failure(
                            &mut attempt_diagnostics,
                            &request_id,
                            attempt.saturating_add(1),
                            &text,
                            AttemptAction::JsonModeFallback,
                            c,
                        );
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    if !response_format_fallback
                        && request.get("response_format").is_some()
                        && response_format_rejected(&text)
                    {
                        request
                            .as_object_mut()
                            .expect("request was validated as an object")
                            .remove("response_format");
                        response_format_fallback = true;
                        record_attempt_failure(
                            &mut attempt_diagnostics,
                            &request_id,
                            attempt.saturating_add(1),
                            &text,
                            AttemptAction::UnstructuredFallback,
                            c,
                        );
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    // A dropped connection or missing terminator is transient
                    // too. Retrying is safe only before any text reached the
                    // user; the partial response is discarded, never merged.
                    let silent = !emitted_text.load(Ordering::Relaxed);
                    let transient = transient_error(&text);
                    // A follow-up answer is asked again with a smaller request
                    // instead; another attempt would wait out the same timeout.
                    let unanswered = !c.retry_timeouts && timeout_error(&text);
                    // Exhaust ordinary transient retries first, then make one
                    // compatibility attempt for an SSE grammar failure. The
                    // caller still validates the complete response schema.
                    // An overload is no grammar failure: after three Nvidia
                    // "Service temporarily overloaded" errors, a live run's
                    // JSON-mode attempt returned an unfinished `{"issues":{}`.
                    if silent
                        && !unanswered
                        && !overload_error(&text)
                        && !schema_stream_fallback
                        && text.starts_with("provider_stream_error:")
                        && transient_retries >= c.retries
                        && downgrade_json_schema(&mut request)
                    {
                        schema_stream_fallback = true;
                        record_attempt_failure(
                            &mut attempt_diagnostics,
                            &request_id,
                            attempt.saturating_add(1),
                            &text,
                            AttemptAction::JsonModeFallback,
                            c,
                        );
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    let retry = silent
                        && !unanswered
                        && (transient
                            || (attempt == 0 && text.starts_with("invalid_tool_arguments:"))
                            || (attempt == 0 && text.starts_with("invalid_stream_event:")));
                    if !retry || transient_retries >= c.retries {
                        record_attempt_failure(
                            &mut attempt_diagnostics,
                            &request_id,
                            attempt.saturating_add(1),
                            &text,
                            AttemptAction::Stop,
                            c,
                        );
                        return Err(CompletionError::new(
                            e,
                            attempt.saturating_add(1),
                            silent && transient,
                        )
                        .with_diagnostics(attempt_diagnostics)
                        .into());
                    }
                    record_attempt_failure(
                        &mut attempt_diagnostics,
                        &request_id,
                        attempt.saturating_add(1),
                        &text,
                        AttemptAction::Retry,
                        c,
                    );
                    transient_retries = transient_retries.saturating_add(1);
                    attempt = attempt.saturating_add(1);
                    tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=tokio::time::sleep(Duration::from_millis(500*(1<<attempt.min(5))))=>{}}
                }
            }
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use crate::config::Secret;

    #[test]
    fn provider_diagnostics_redact_credentials_before_clipping() {
        let key = "test_api_key";
        let config = Config {
            api_key: Some(Secret(key.into())),
            ..Config::compact_test()
        };
        let mut diagnostics = Vec::new();
        for error in [
            format!("http_503: rejected {key}\n{}", "긴 오류 메시지".repeat(150)),
            key.into(),
        ] {
            record_attempt_failure(
                &mut diagnostics,
                "redaction-test",
                1,
                &error,
                AttemptAction::Retry,
                &config,
            );
        }
        assert!(!json!(diagnostics).to_string().contains(key));
        assert!(
            diagnostics[0]
                .reason
                .starts_with("http_503: rejected [redacted]")
        );
        assert_eq!(diagnostics[0].reason.chars().count(), MAX_DIAGNOSTIC_CHARS);
        assert_eq!(diagnostics[1].code, "request_error");
        assert_eq!(diagnostics[1].reason, "[redacted]");
    }
}
