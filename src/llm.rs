use crate::config::Config;
use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: usize,
    pub output: usize,
    pub cached: Option<usize>,
}
#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub attempts: usize,
    pub length_limited: bool,
    pub discarded_tool_calls: bool,
}

/// The provider may retry a request several times before returning an error.
/// Keep that count attached to the error so the agent can account for every
/// attempted request in its run budget instead of charging only the final one.
#[derive(Debug)]
pub(crate) struct CompletionError {
    source: anyhow::Error,
    attempts: usize,
}
impl CompletionError {
    fn new(source: anyhow::Error, attempts: usize) -> Self {
        Self { source, attempts }
    }

    pub(crate) fn attempts(&self) -> usize {
        self.attempts.max(1)
    }
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

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}
impl SseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > 8 * 1024 * 1024 {
            bail!("SSE event too large");
        }
        let mut result = vec![];
        loop {
            let lf = self
                .buffer
                .windows(2)
                .position(|x| x == b"\n\n")
                .map(|p| (p, 2));
            let crlf = self
                .buffer
                .windows(4)
                .position(|x| x == b"\r\n\r\n")
                .map(|p| (p, 4));
            let Some((pos, size)) = lf.into_iter().chain(crlf).min_by_key(|(p, _)| *p) else {
                break;
            };
            let event = String::from_utf8(self.buffer.drain(..pos + size).collect())?;
            let data = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                result.push(data)
            }
        }
        Ok(result)
    }
}
impl OpenAiClient {
    fn client(c: &Config) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder().connect_timeout(Duration::from_secs(15));
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
                dotenvy::from_path_iter(".env")
                    .ok()?
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
        let mut req = Self::client(c)?
            .post(format!(
                "{}/chat/completions",
                c.base_url.trim_end_matches('/')
            ))
            .json(&request);
        if let Some(key) = Self::key(c) {
            req = req.bearer_auth(key)
        }
        let response = tokio::select! {_ = cancel.cancelled()=>bail!("cancelled"),r=req.send()=>r?};
        if !response.status().is_success() {
            let status = response.status();
            // Error responses can have a slow or unbounded body too. Keep
            // cancellation responsive while collecting a bounded diagnostic.
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while bytes.len() < MAX_ERROR_BODY_BYTES {
                let next = tokio::select! {
                    _ = cancel.cancelled() => bail!("cancelled"),
                    chunk = stream.next() => chunk,
                };
                let Some(chunk) = next else {
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
        let (mut done, mut finish) = (false, false);
        loop {
            let next = tokio::select! {_ = cancel.cancelled()=>bail!("cancelled"),chunk=stream.next()=>chunk};
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| anyhow::anyhow!("stream_interrupted: {e}"))?;
            for event in parser.feed(&chunk)? {
                if event == "[DONE]" {
                    done = true;
                    continue;
                }
                let v: Value = serde_json::from_str(&event)
                    .map_err(|e| anyhow::anyhow!("invalid_stream_event: {e}"))?;
                if !v["error"].is_null() {
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
                if let Some(reason) = choice["finish_reason"].as_str() {
                    if !["stop", "tool_calls", "length"].contains(&reason) {
                        if reason == "error" {
                            bail!("provider_stream_error: finish_reason=error");
                        }
                        bail!("incomplete_completion: {reason}");
                    }
                    out.length_limited = reason == "length";
                    finish = true;
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
                    }
                }
                if out.text.len() + calls.values().map(|c| c.arguments.len()).sum::<usize>()
                    > 8 * 1024 * 1024
                {
                    bail!("response_size_limit");
                }
            }
            if done {
                break;
            }
        }
        if !done || !finish {
            bail!("stream_interrupted: missing completion terminator");
        }
        if out.length_limited {
            // A length-limited batch may contain syntactically valid but unfinished
            // instructions. Never expose any of its calls for execution.
            out.discarded_tool_calls = !calls.is_empty();
            return Ok(out);
        }
        let mut ids = std::collections::BTreeSet::new();
        for call in calls.values() {
            if call.id.is_empty() || call.name.is_empty() || !ids.insert(call.id.clone()) {
                bail!("malformed_tool_call");
            }
            let _: serde_json::Map<String, Value> = serde_json::from_str(&call.arguments)
                .map_err(|e| anyhow::anyhow!("invalid_tool_arguments: {e}"))?;
        }
        out.calls = calls.into_values().collect();
        Ok(out)
    }
    pub async fn probe(&self, c: &Config) -> Result<String> {
        c.runnable()?;
        let mut body = json!({"model":c.model,"messages":[{"role":"user","content":"Reply OK"}],"stream":false});
        body[if c.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        }] = json!(c.output_tokens);
        apply_thinking_settings(&mut body, c);
        let mut req = Self::client(c)?
            .post(format!(
                "{}/chat/completions",
                c.base_url.trim_end_matches('/')
            ))
            .json(&body);
        if let Some(k) = Self::key(c) {
            req = req.bearer_auth(k)
        }
        let plain =
            tokio::time::timeout(Duration::from_secs(c.request_timeout_secs), req.send()).await??;
        if !plain.status().is_success() {
            bail!("plain response probe failed: {}", plain.status());
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
        request.as_object_mut().unwrap().remove("tool_choice");
        let call = &completion.calls[0];
        request["messages"].as_array_mut().unwrap().extend([json!({"role":"assistant","content":null,"tool_calls":[{"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}]}),json!({"role":"tool","tool_call_id":call.id,"content":"OK"})]);
        let final_response = self
            .complete(request, c, CancellationToken::new(), tx)
            .await?;
        if final_response.length_limited {
            bail!("incomplete_completion: length during connection probe");
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
        let emitted_text = Arc::new(AtomicBool::new(false));
        let mut attempt = 0usize;
        // The optional response-format fallback is a compatibility attempt,
        // not one of the configured transient retries. Keep its attempt in
        // the usage accounting, but track retry budget separately so a
        // fallback cannot either consume all retries or make retries=0 loop
        // forever after a subsequent 429/5xx response.
        let mut transient_retries = 0usize;
        let mut response_format_fallback = false;
        loop {
            // Cancellation must cover every await inside an attempt, including
            // response-body reads and backpressure on the delta channel.
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("cancelled"),
                    result = tokio::time::timeout(
                        Duration::from_secs(c.request_timeout_secs),
                        self.attempt(
                            request.clone(),
                            c,
                            cancel.clone(),
                            delta.clone(),
                            emitted_text.clone(),
                            stream_deltas,
                        ),
                    ) => result,
            };
            let result = match result {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("request_timeout")),
            };
            match result {
                Ok(mut r) => {
                    r.attempts = attempt.saturating_add(1);
                    return Ok(r);
                }
                Err(e) => {
                    let text = e.to_string();
                    // OpenAI-compatible local servers are not uniform about
                    // JSON mode. Retry once without the optional structured
                    // output hint when the server explicitly rejects it;
                    // finish() still validates the returned object.
                    if !response_format_fallback
                        && request.get("response_format").is_some()
                        && response_format_rejected(&text)
                    {
                        request
                            .as_object_mut()
                            .expect("request was validated as an object")
                            .remove("response_format");
                        response_format_fallback = true;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    let retry = !emitted_text.load(Ordering::Relaxed)
                        && (text.starts_with("provider_stream_error:")
                            || (attempt == 0 && text.starts_with("invalid_tool_arguments:"))
                            || (attempt == 0 && text.starts_with("invalid_stream_event:"))
                            || text.starts_with("http_429")
                            || text.starts_with("http_5")
                            || text.contains("error sending request"));
                    if !retry || transient_retries >= c.retries {
                        return Err(CompletionError::new(e, attempt.saturating_add(1)).into());
                    }
                    transient_retries = transient_retries.saturating_add(1);
                    attempt = attempt.saturating_add(1);
                    tokio::select! {_=cancel.cancelled()=>bail!("cancelled"),_=tokio::time::sleep(Duration::from_millis(500*(1<<attempt.min(5))))=>{}}
                }
            }
        }
    }
}
