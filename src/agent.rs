use crate::{
    config::Config,
    context::{self, ContextManager},
    llm::{LlmClient, OpenAiClient, ToolCall},
    session::Session,
    tools::{self, ToolRegistry},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub enum RunCommand {
    Configure(Box<Config>),
    Tools(std::collections::BTreeSet<String>),
}

#[derive(Clone, Debug)]
pub enum AgentEvent {
    Delta {
        session: String,
        text: String,
    },
    Snapshot(Box<Session>),
    Tool {
        session: String,
        name: String,
        status: String,
    },
    Notice {
        session: String,
        text: String,
    },
}
async fn snapshot(s: &Session, tx: &mpsc::Sender<AgentEvent>) {
    let _ = tx.send(AgentEvent::Snapshot(Box::new(s.clone()))).await;
}
fn assistant(text: &str, calls: &[ToolCall]) -> Value {
    let mut v = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        v["tool_calls"]=json!(calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}})).collect::<Vec<_>>());
    }
    v
}
async fn execute_one(
    mut s: Session,
    call: ToolCall,
    cancel: &CancellationToken,
) -> (Session, Value) {
    if cancel.is_cancelled() {
        return (s, tools::envelope(Err(anyhow::anyhow!("cancelled"))));
    }
    let timeout = s.config.tool_timeout_secs;
    let child = cancel.child_token();
    let tool_cancel = child.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        let result = tools::run_call_cancellable(&mut s, &call, &tool_cancel);
        (s, result)
    });
    // Joining a mutating operation is mandatory: cancellation must not hide a completed write.
    match tokio::time::timeout(Duration::from_secs(timeout), &mut job).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => panic!("tool worker panicked: {e}"),
        Err(_) => {
            child.cancel();
            let (mut s, result) = job.await.expect("tool worker panicked");
            s.last_error = Some(
                "Tool exceeded deadline; waited for its final outcome to avoid an untracked write"
                    .into(),
            );
            (s, result)
        }
    }
}
async fn read_parallel(
    s: &mut Session,
    calls: &[ToolCall],
    cancel: &CancellationToken,
) -> Vec<Value> {
    use futures_util::{StreamExt, stream};
    let project = s.project.clone();
    let config = s.config.clone();
    let active = s.active_tools.clone();
    let history = crate::session::SessionHistory {
        bundles: s
            .history
            .bundles
            .iter()
            .filter(|b| b.active)
            .cloned()
            .collect(),
        next_id: s.history.next_id,
        pruned_through: s.history.pruned_through,
    };
    let guidance = s.run_guidance.clone();
    let owned_calls = calls.to_vec();
    let futures=owned_calls.into_iter().map(|call|{let mut temporary=Session::new(project.clone(),config.clone());temporary.active_tools=active.clone();temporary.history=history.clone();temporary.run_guidance=guidance.clone();let cancel=cancel.clone();let timeout=config.tool_timeout_secs;async move{
        let child=cancel.child_token();let tool_cancel=child.clone();let mut job=tokio::task::spawn_blocking(move||{let result=tools::run_call_cancellable(&mut temporary,&call,&tool_cancel);(temporary,result)});
        tokio::select!{_ = cancel.cancelled()=>{child.cancel();Err(anyhow::anyhow!("cancelled"))},result=tokio::time::timeout(Duration::from_secs(timeout),&mut job)=>match result {Ok(Ok(value))=>Ok(value),Ok(Err(e))=>Err(anyhow::anyhow!("tool worker failed: {e}")),Err(_)=>{child.cancel();Err(anyhow::anyhow!("tool_timeout"))}}}
    }});
    let mut results = vec![];
    let mut pending = stream::iter(futures).buffered(config.read_parallelism);
    while let Some(result) = pending.next().await {
        match result {
            Ok((temp, mut result)) => {
                for source in temp.sources.into_values() {
                    if let Some(path) = &source.path {
                        s.memory.stale_path(path, source.hash.as_deref());
                    }
                    s.sources.insert(source.id.clone(), source);
                }
                // Temporary history IDs cannot escape into the owning session.
                if result["truncated"] == true
                    && let Some(id) = result["archive_id"]
                        .as_u64()
                        .or_else(|| result["next_cursor"]["id"].as_u64())
                    && let Ok(bundle) = temp.history.read(id)
                {
                    let id = s.history.push(bundle.messages.clone(), true);
                    s.history.bundles.back_mut().unwrap().active = false;
                    result["archive_id"] = json!(id);
                    if result["next_cursor"]["tool"] == "history" {
                        result["next_cursor"]["id"] = json!(id);
                    }
                }
                results.push(result)
            }
            Err(e) => results.push(tools::envelope(Err(e))),
        }
    }
    results
}

pub async fn run_session(
    s: Session,
    client: Arc<dyn LlmClient>,
    cancel: CancellationToken,
    events: mpsc::Sender<AgentEvent>,
) -> Session {
    let (_tx, rx) = mpsc::channel(1);
    run_session_controlled(s, client, cancel, events, rx).await
}
pub async fn run_session_controlled(
    mut s: Session,
    client: Arc<dyn LlmClient>,
    cancel: CancellationToken,
    events: mpsc::Sender<AgentEvent>,
    mut commands: mpsc::Receiver<RunCommand>,
) -> Session {
    s.status = "running".into();
    s.last_error = None;
    let started = Instant::now();
    let initial_tokens = s.input_tokens + s.output_tokens;
    let mut failure = None;
    let mut finalization_attempts = 0usize;
    let mut repetitions = std::collections::BTreeMap::<String, usize>::new();
    let mut last_document_hash: Option<String> = None;
    let mut verified_count = s
        .investigations
        .iter()
        .filter(|i| i.status == "verified")
        .count();
    if let Err(e) = s.config.runnable() {
        failure = Some(e.to_string());
    }
    snapshot(&s, &events).await;
    while failure.is_none() && !cancel.is_cancelled() {
        while let Ok(command) = commands.try_recv() {
            match command {
                RunCommand::Configure(config) => s.pending_config = Some(*config),
                RunCommand::Tools(names) => s.active_tools = names,
            }
        }
        if let Some(config) = s.pending_config.clone() {
            let _ = s.history.prune(config.history_bytes);
            match apply_config(&mut s, config) {
                Ok(()) => {
                    s.pending_config = None;
                    let _ = events
                        .send(AgentEvent::Notice {
                            session: s.id.clone(),
                            text: "Settings applied at request boundary".into(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = events
                        .send(AgentEvent::Notice {
                            session: s.id.clone(),
                            text: format!("Settings pending cleanup: {error}"),
                        })
                        .await;
                }
            }
        }
        if started.elapsed().as_secs() >= s.config.run_timeout_secs
            || s.input_tokens + s.output_tokens - initial_tokens >= s.config.run_tokens
        {
            failure = Some("run_budget_exhausted: partial results and memory retained".into());
            break;
        }
        let spent = (s.input_tokens + s.output_tokens).saturating_sub(initial_tokens);
        let remaining = s.config.run_tokens.saturating_sub(spent);
        let seconds_remaining = s
            .config
            .run_timeout_secs
            .saturating_sub(started.elapsed().as_secs());
        let fraction = (remaining as f64 / s.config.run_tokens as f64)
            .min(seconds_remaining as f64 / s.config.run_timeout_secs as f64);
        let phase = if finalization_attempts > 0 || fraction <= s.config.verification_reserve_ratio
        {
            "verify"
        } else if fraction <= s.config.writing_reserve_ratio {
            "draft"
        } else {
            "investigate"
        };
        s.run_guidance = json!({"finalization_attempts":finalization_attempts,"phase":phase,"remaining_tokens":remaining,"remaining_seconds":seconds_remaining,
            "writing_reserve_tokens":(s.config.run_tokens as f64*s.config.writing_reserve_ratio) as usize,
            "verification_reserve_tokens":(s.config.run_tokens as f64*s.config.verification_reserve_ratio) as usize,
            "pending_count":s.investigations.iter().filter(|i|i.status != "verified").count(),
            "instruction":match phase {"verify"=>"Stop expanding scope. Audit and verify existing sections, fix factual errors, then report remaining unknowns.","draft"=>"Write investigated sections now; use targeted reads only. Preserve verification budget.",_=>"Investigate incrementally and write each completed section."}});
        let definitions = ToolRegistry::definitions(&s);
        let mut request = match ContextManager::request(&s, definitions.clone()) {
            Ok(r) => r,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        let estimate = context::count(&request, &s.config.model);
        if let Err(e) = ContextManager::prepare(&mut s, estimate) {
            failure = Some(e.to_string());
            break;
        }
        if let Some(cp) = &mut s.checkpoint {
            if cp.attempts >= 3 {
                failure = Some("checkpoint_retry_limit: original context retained".into());
                break;
            }
            cp.failed = false;
            cp.acknowledged = false;
            request = match ContextManager::request(&s, ToolRegistry::definitions(&s)) {
                Ok(r) => r,
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            };
        }
        let request_tokens = context::count(&request, &s.config.model);
        // Reasoning models spend output tokens on reasoning too; retain the
        // configured output allowance and reserve it for every cleanup round.
        let request_config = s.config.clone();
        if request_tokens + request_config.output_tokens + 512 > s.config.context_tokens {
            failure = Some(format!(
                "context_limit: input estimate {request_tokens} + output {} + margin 512 exceeds {}; original context retained",
                request_config.output_tokens, s.config.context_tokens
            ));
            break;
        }
        if s.input_tokens + s.output_tokens - initial_tokens
            + request_tokens
            + request_config.output_tokens
            > s.config.run_tokens
        {
            failure = Some("run_budget_exhausted: insufficient budget for next request".into());
            break;
        }
        if let Some(cp) = &mut s.checkpoint {
            cp.attempts += 1;
        }
        snapshot(&s, &events).await;
        let (tx, mut rx) = mpsc::channel(64);
        let event_tx = events.clone();
        let sid = s.id.clone();
        let relay = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let _ = event_tx
                    .send(AgentEvent::Delta {
                        session: sid.clone(),
                        text,
                    })
                    .await;
            }
        });
        let response = tokio::select! {_ = cancel.cancelled()=>Err(anyhow::anyhow!("cancelled")),result=tokio::time::timeout(Duration::from_secs(s.config.run_timeout_secs.saturating_sub(started.elapsed().as_secs()).max(1)),client.complete(request,&request_config,cancel.clone(),tx))=>match result{Ok(r)=>r,Err(_)=>Err(anyhow::anyhow!("run_timeout"))}};
        let _ = relay.await;
        let completion = match response {
            Ok(r) => r,
            Err(e) => {
                s.usage_incomplete = true;
                s.input_tokens += request_tokens;
                failure = Some(e.to_string());
                break;
            }
        };
        if completion.attempts > 1 {
            s.usage_incomplete = true;
            s.input_tokens += request_tokens * (completion.attempts - 1);
        }
        if let Some(u) = completion.usage {
            s.input_tokens += u.input;
            s.output_tokens += u.output;
            if let Some(c) = u.cached {
                s.cached_tokens = Some(s.cached_tokens.unwrap_or(0) + c);
            }
        } else {
            s.usage_incomplete = true;
            s.input_tokens += request_tokens;
            s.output_tokens += context::tokens(&completion.text, &s.config.model)
                + completion
                    .calls
                    .iter()
                    .map(|c| context::tokens(&c.arguments, &s.config.model))
                    .sum::<usize>();
        }
        let batch_limit = if s.checkpoint.is_some() {
            ContextManager::cleanup_result_budget(&s.config)
        } else {
            s.config.batch_tokens
        };
        let call_limit = 32.min(batch_limit / 200);
        if completion.calls.len() > call_limit {
            failure = Some(format!(
                "tool_call_batch_limit: maximum {call_limit} calls at this result budget"
            ));
            break;
        }
        if completion.calls.is_empty() {
            let id = s.history.push(vec![assistant(&completion.text, &[])], true);
            if let Some(cp) = &mut s.checkpoint {
                cp.maintenance_bundle_ids.push(id);
                continue;
            }
            if s.pending_config.is_some() {
                failure =
                    Some("Pending settings still require cleanup; old settings retained".into());
                break;
            }
            if s.document_written && s.investigations.is_empty() {
                s.status = "partial".into();
                s.last_error = Some("Document has no verified investigation coverage".into());
            } else if !s.investigations.is_empty() {
                let _ = tools::revalidate(&mut s);
                if s.investigations.iter().any(|i| i.status != "verified") {
                    s.status = "partial".into();
                    s.last_error =
                        Some("Unverified investigation items remain; document is partial".into());
                } else {
                    match tools::audit_document(&mut s) {
                        Ok(audit) if audit["structural_ok"] == true => {
                            s.status = "complete".into();
                            s.last_error = None;
                        }
                        Ok(audit) => {
                            s.status = "partial".into();
                            s.last_error = Some(format!(
                                "Document evidence audit has {} issues; use document_audit",
                                audit["issue_count"]
                            ));
                        }
                        Err(error) => {
                            s.status = "partial".into();
                            s.last_error = Some(format!("Document audit failed: {error}"));
                        }
                    }
                }
            } else {
                s.status = "complete".into();
            }
            if s.status == "partial" && finalization_attempts < s.config.review_limit.min(2) {
                finalization_attempts += 1;
                s.status = "running".into();
                let _ = events.send(AgentEvent::Notice { session:s.id.clone(), text:"Completion checks failed; returning to pending evidence verification within the remaining budget".into() }).await;
                continue;
            }
            break;
        }
        let mut messages = vec![assistant(&completion.text, &completion.calls)];
        // A checkpoint acknowledges the whole batch. Evaluate it after writes,
        // even if a provider emits its call first; preserve other write ordering.
        let mut execution_calls = completion.calls.clone();
        execution_calls.sort_by_key(|call| call.name == "checkpoint_complete");
        let mut i = 0;
        let mut remaining = batch_limit;
        while i < execution_calls.len() {
            let call = &execution_calls[i];
            let parallel = ["file_list", "file_read", "source_search"]
                .contains(&call.name.as_str())
                && s.active_tools.contains(&call.name)
                && s.checkpoint.is_none()
                && s.run_guidance["phase"] != "verify";
            let mut group = 1;
            if parallel {
                while i + group < execution_calls.len()
                    && ["file_list", "file_read", "source_search"]
                        .contains(&execution_calls[i + group].name.as_str())
                    && s.active_tools.contains(&execution_calls[i + group].name)
                {
                    group += 1
                }
            }
            let results = if parallel {
                read_parallel(&mut s, &execution_calls[i..i + group], &cancel).await
            } else {
                let (next, result) = execute_one(s, call.clone(), &cancel).await;
                s = next;
                vec![result]
            };
            for (call, result) in execution_calls[i..i + group].iter().zip(results) {
                if call.name == "document_edit"
                    && result["status"] == "ok"
                    && let Some(digest) = result["data"]["hash"].as_str()
                    && last_document_hash.as_deref() != Some(digest)
                {
                    repetitions.clear();
                    last_document_hash = Some(digest.into());
                }
                if [
                    "file_read",
                    "file_list",
                    "source_search",
                    "symbol_search",
                    "document_inspect",
                ]
                .contains(&call.name.as_str())
                    || result["status"] != "ok"
                {
                    let args =
                        serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
                    let signature = format!("{}:{}:{}", call.name, args, result["data"]["hash"]);
                    let count = repetitions.entry(signature).or_default();
                    *count += 1;
                    if *count >= s.config.stall_round_limit {
                        failure = Some(format!(
                            "repeated_work_limit: {} repeated {} times without document progress; results retained. Resume with a narrower instruction.",
                            call.name, count
                        ));
                    }
                }

                if result["status"] != "ok"
                    && let Some(cp) = &mut s.checkpoint
                {
                    cp.failed = true;
                    cp.acknowledged = false;
                }
                let slots = execution_calls.len() - messages.len() + 1;
                let budget = remaining
                    .saturating_sub(slots.saturating_sub(1) * 200)
                    .min(s.config.result_tokens)
                    .max(200);
                let result = tools::limit_result(&mut s, call, result, budget);
                remaining =
                    remaining.saturating_sub(tools::result_tokens(call, &result, &s.config.model));
                let _ = events
                    .send(AgentEvent::Tool {
                        session: s.id.clone(),
                        name: call.name.clone(),
                        status: result["status"].as_str().unwrap_or("error").into(),
                    })
                    .await;
                messages.push(
                    json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()}),
                );
            }
            i += group;
        }
        let new_verified = s
            .investigations
            .iter()
            .filter(|i| i.status == "verified")
            .count();
        if new_verified > verified_count {
            repetitions.clear();
            if failure
                .as_deref()
                .is_some_and(|e| e.starts_with("repeated_work_limit"))
            {
                failure = None;
            }
        }
        verified_count = new_verified;
        let id = s.history.push(messages, true);
        if let Some(cp) = &mut s.checkpoint {
            cp.maintenance_bundle_ids.push(id);
        }
        if let Some(pending) = s.pending_tools.take() {
            s.active_tools = pending;
        }
        if cancel.is_cancelled() {
            if let Some(cp) = &mut s.checkpoint {
                cp.acknowledged = false;
            }
            break;
        }
        if s.checkpoint
            .as_ref()
            .is_some_and(|c| c.acknowledged && !c.failed)
            && let Err(e) = ContextManager::commit(&mut s)
        {
            failure = Some(e.to_string());
            break;
        }
        if s.history.bytes() > s.config.history_bytes * 2 {
            failure = Some("history_hard_limit: cleanup required".into());
            break;
        }
        // Bound ancillary session data too; never silently discard observations or receipts.
        if serde_json::to_vec(&(&s.sources, &s.ledger, &s.investigations, &s.task))
            .map_or(true, |v| v.len() > s.config.memory_bytes)
        {
            failure = Some(
                "session_metadata_capacity: start another session or reduce retained details"
                    .into(),
            );
            break;
        }
        snapshot(&s, &events).await;
    }
    if cancel.is_cancelled() {
        s.status = "cancelled".into();
        s.last_error = None;
    } else if let Some(error) = failure {
        s.status = "blocked".into();
        s.last_error = Some(error);
    }
    snapshot(&s, &events).await;
    s
}

pub async fn headless(mut s: Session, prompt: String) -> Result<Session> {
    s.add_user(prompt);
    let (tx, mut rx) = mpsc::channel(128);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let listener = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        c.cancel();
    });
    let renderer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Delta { text, .. } => {
                    print!("{text}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::Tool { name, status, .. } => eprintln!("\n[{name}: {status}]"),
                _ => {}
            }
        }
    });
    let result = run_session(s, Arc::new(OpenAiClient), cancel, tx).await;
    listener.abort();
    let _ = renderer.await;
    eprintln!(
        "\nStatus: {}\nOutput: {}",
        result.status,
        result.project.output.display()
    );
    if let Some(e) = &result.last_error {
        eprintln!("{e}");
    }
    Ok(result)
}
pub fn apply_config(s: &mut Session, config: Config) -> Result<()> {
    s.check_limits(&config)?;
    let mut copy = s.clone();
    copy.config = config;
    ContextManager::state(&copy)?;
    let request = ContextManager::request(&copy, ToolRegistry::definitions(&copy))?;
    if context::count(&request, &copy.config.model) + copy.config.output_tokens
        > copy.config.context_tokens
    {
        bail!("New context budget requires checkpoint first; current settings retained");
    }
    s.config = copy.config;
    Ok(())
}
