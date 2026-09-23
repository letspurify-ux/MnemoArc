use crate::{
    config::Config,
    context::{self, ContextManager},
    llm::{LlmClient, OpenAiClient, ToolCall},
    session::Session,
    tools::{self, ToolRegistry},
};
use anyhow::{Result, bail};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const FINALIZATION_RETRY_LIMIT: usize = 8;
const COMPLETION_REVIEW_NO_PROGRESS_LIMIT: usize = 3;
const COMPLETION_REVIEW_EXHAUST_LIMIT: usize = 6;
const COMPLETION_REPAIR_ROUND_LIMIT: usize = 16;
const COMPLETION_REPAIR_EXHAUST_LIMIT: usize = 32;
const LENGTH_RECOVERY_LIMIT: usize = 8;
const REVIEW_RESPONSE_LIMIT: usize = 8;

fn repeated_outcome_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(4).max(12)
}

fn repeated_outcome_exhausted(s: &Session) -> bool {
    s.progress_recovery.repeated_outcome_rounds >= repeated_outcome_limit(&s.config)
}

fn substantive_progress_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(8).max(24)
}

fn artifact_focus_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(4).max(16)
}

fn artifact_exhaust_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(8).max(32)
}

const REPEATED_OUTCOME_ERROR: &str = "progress_recovery_exhausted: repeated responses or tool results produced no new output, source evidence or verified result; current work is retained for a changed approach";
const NAVIGATION_STALL_ERROR: &str = "progress_recovery_exhausted: navigation and bookkeeping did not produce source evidence, a changed artifact or a verified result; current work is retained for a changed approach";
const ARTIFACT_CHURN_ERROR: &str = "artifact_progress_exhausted: many distinct edits produced no completed task item, verified section or improved completion check; current files and requirements are retained for a changed approach";

/// Document recovery is bounded by the run budget, not an independent retry
/// quota. Keep the cause visible so the next request must change its approach.
fn recover_document(s: &mut Session, reason: &str) -> bool {
    if !s.is_document_work() {
        return false;
    }
    s.progress_recovery.recovery_reason = Some(reason.into());
    true
}

/// A tool-free reviewer cannot repair a missing citation, an oversized draft,
/// or invalid metadata. Return such failures to the agent with the cause intact.
fn recover_review_setup(s: &mut Session, reason: &str) -> bool {
    if !s.is_document_work()
        || !tools::recovery::correctable_document_error(&json!({
            "recovery": tools::recovery::describe(reason)
        }))
    {
        return false;
    }
    if s.document_review.pending {
        tools::document_review::defer_for_repair(s);
    }
    s.completion_review.pending = false;
    s.completion_review.approved = false;
    s.status = "running".into();
    s.last_error = Some(reason.into());
    s.progress_recovery.action_required = !reason.starts_with("completion_review_budget:");
    recover_document(
        s,
        &format!(
            "Review preparation failed: {reason}. Correct the cited content, missing evidence or task metadata with tools, or shorten only the final chat report if it exceeded review input. Preserve the original requirements and requested document content, then request final verification again."
        ),
    )
}

fn recover_unexecuted_batch(s: &mut Session, reason: &str) -> bool {
    let code = reason.split(':').next().unwrap_or(reason);
    if !s.is_document_work()
        || !matches!(
            code,
            "tool_call_batch_limit" | "invalid_tool_arguments" | "malformed_tool_call"
        )
    {
        return false;
    }
    // The provider parser and completion validator reject the whole response
    // before execution. Recover malformed calls just like oversized batches;
    // they must not bypass the tool executor's correctable-input policy.
    // Review requests have no executable tools. Their next request needs JSON
    // protocol feedback, not an instruction to reissue a smaller tool batch.
    if s.checkpoint.is_none() && s.completion_review.pending {
        s.last_error = Some(format!(
            "completion_review_invalid: {reason}; return complete checks JSON without tool calls"
        ));
        return true;
    }
    if s.checkpoint.is_none() && s.document_review.pending {
        s.last_error = Some(format!(
            "document_review_incomplete: {reason}; return complete issues JSON without tool calls"
        ));
        return true;
    }
    let budget = if s.checkpoint.is_some() {
        ContextManager::cleanup_result_budget(&s.config)
    } else {
        s.config.batch_tokens
    };
    let limit = crate::llm::MAX_TOOL_CALLS.min(budget / 200);
    let guidance = format!(
        "{reason}; none of this batch executed. Reissue necessary calls in batches of at most {limit} with valid JSON object arguments, short unique call IDs and exact available tool names."
    );
    s.last_error = Some(reason.into());
    s.progress_recovery.action_required = true;
    recover_document(s, &guidance)
}

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
// A detached relay can retain the web event sender and prevent shutdown.
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn run_deadline(started: Instant, config: &Config) -> tokio::time::Instant {
    tokio::time::Instant::from_std(started)
        .checked_add(Duration::from_secs(config.run_timeout_secs))
        .unwrap_or_else(tokio::time::Instant::now)
}

fn same_source(a: &crate::memory::Source, b: &crate::memory::Source) -> bool {
    a.origin == b.origin
        && a.path == b.path
        && a.start_line == b.start_line
        && a.end_line == b.end_line
        && a.line_start_complete == b.line_start_complete
        && a.line_end_complete == b.line_end_complete
        && a.evidence_truncated == b.evidence_truncated
        && a.hash == b.hash
        && a.excerpt == b.excerpt
}

fn remap_source_ids(value: &mut Value, ids: &std::collections::BTreeMap<String, String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                remap_source_ids(value, ids);
            }
        }
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if matches!(key.as_str(), "source" | "signature_source")
                    && let Some(source) = value.as_object_mut()
                    && let Some(id) = source.get("id").and_then(Value::as_str).map(str::to_owned)
                    && let Some(canonical) = ids.get(&id)
                {
                    source.insert("id".into(), json!(canonical));
                }
                remap_source_ids(value, ids);
            }
        }
        _ => {}
    }
}

async fn emit(
    tx: &mpsc::Sender<AgentEvent>,
    event: AgentEvent,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { let _ = tx.try_send(event); }
        _ = tokio::time::sleep_until(deadline) => { let _ = tx.try_send(event); }
        permit = tx.reserve() => { if let Ok(permit) = permit { permit.send(event); } }
    }
}
async fn snapshot(
    s: &Session,
    tx: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) {
    emit(
        tx,
        AgentEvent::Snapshot(Box::new(s.clone())),
        cancel,
        deadline,
    )
    .await;
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
    let backup = s.clone();
    let child = cancel.child_token();
    let tool_cancel = child.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        let result = tools::run_call_cancellable(&mut s, &call, &tool_cancel);
        (s, result)
    });
    // Joining a mutating operation is mandatory: cancellation must not hide a completed write.
    match tokio::time::timeout(Duration::from_secs(timeout), &mut job).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => (
            backup,
            tools::envelope(Err(anyhow::anyhow!(
                "tool_worker_panic: worker lost; last snapshot retained, write outcomes require review"
            ))),
        ),
        Err(_) => {
            child.cancel();
            let (mut s, result) = match job.await {
                Ok(result) => result,
                Err(_) => {
                    return (
                        backup,
                        tools::envelope(Err(anyhow::anyhow!(
                            "tool_worker_panic: worker lost after deadline; write outcomes require review"
                        ))),
                    );
                }
            };
            s.last_error = Some(
                "Tool exceeded deadline; waited for its final outcome to avoid an untracked write"
                    .into(),
            );
            (s, result)
        }
    }
}

fn rebase_document_call(call: &ToolCall, hash: Option<&str>) -> (ToolCall, bool) {
    let Some(hash) = hash else {
        return (call.clone(), false);
    };
    if !matches!(call.name.as_str(), "document_edit" | "document_edit_batch") {
        return (call.clone(), false);
    }
    let Ok(mut args) = serde_json::from_str::<Value>(&call.arguments) else {
        return (call.clone(), false);
    };
    let Some(object) = args.as_object_mut() else {
        // Malformed non-object arguments must reach normal tool validation;
        // never index them mutably while attempting hash recovery.
        return (call.clone(), false);
    };
    let action = object.get("action").and_then(Value::as_str).unwrap_or("");
    // A missing precondition can be recovered after an earlier successful
    // edit in this response, but an explicitly supplied non-string value is
    // malformed input and must still reach normal schema validation. Treating
    // null/numbers as omitted would silently turn a bad call into a write.
    let expected_hash = match object.get("expected_hash") {
        None => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        Some(Value::String(_)) => return (call.clone(), false),
        Some(_) => return (call.clone(), false),
    };
    // A create call intentionally has no revision precondition: rebasing it
    // would turn its useful document_exists error into a less meaningful
    // stale-hash error. A first write on a missing file, however, may be
    // followed by another write in the same response, so fill its now
    // required precondition just like append/patch/section calls.
    if action == "create" || expected_hash == Some(hash) {
        return (call.clone(), false);
    }
    // Every other edit targets the current document. If the model omitted
    // the precondition after an earlier successful edit in this response,
    // supply the chained hash for both the single-call and batch contracts.
    // Invalid/unknown actions still go through normal validation below.
    object.insert("expected_hash".into(), json!(hash));
    let Ok(arguments) = serde_json::to_string(&args) else {
        return (call.clone(), false);
    };
    let mut rebased = call.clone();
    rebased.arguments = arguments;
    (rebased, true)
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
    let file_cursors = s.file_cursors.clone();
    let owned_calls = calls.to_vec();
    let futures = owned_calls.into_iter().map(|call| {
        let mut temporary = Session::new(project.clone(), config.clone());
        temporary.active_tools = active.clone();
        temporary.history = history.clone();
        temporary.run_guidance = guidance.clone();
        temporary.file_cursors = file_cursors.clone();
        let cancel = cancel.clone();
        let timeout = config.tool_timeout_secs;
        async move {
            let child = cancel.child_token();
            let tool_cancel = child.clone();
            let mut job = tokio::task::spawn_blocking(move || {
                let result = tools::run_call_cancellable(&mut temporary, &call, &tool_cancel);
                (temporary, result)
            });
            tokio::select! {
                _ = cancel.cancelled() => {
                    child.cancel();
                    // Do not detach a blocking worker on cancellation. It only
                    // owns a temporary session, but the worker still consumes a
                    // thread and can outlive the run indefinitely otherwise.
                    let _ = job.await;
                    Err(anyhow::anyhow!("cancelled"))
                }
                result = tokio::time::timeout(Duration::from_secs(timeout), &mut job) => {
                    match result {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(error)) => Err(anyhow::anyhow!("tool_worker_panic: tool worker failed: {error}")),
                        Err(_) => {
                            child.cancel();
                            // Reads have no owner-session writes, so the final
                            // outcome can be discarded after the worker joins.
                            let _ = job.await;
                            Err(anyhow::anyhow!("tool_timeout"))
                        }
                    }
                }
            }
        }
    });
    let mut results = vec![];
    let mut pending = stream::iter(futures).buffered(config.read_parallelism);
    while let Some(result) = pending.next().await {
        match result {
            Ok((temp, mut result)) => {
                s.file_cursors.extend(temp.file_cursors);
                let mut source_ids = std::collections::BTreeMap::new();
                for source in temp.sources.into_values() {
                    let source_id = source.id.clone();
                    let canonical = s
                        .sources
                        .values()
                        .find(|existing| same_source(existing, &source))
                        .map(|existing| existing.id.clone());
                    let id = if let Some(id) = canonical {
                        id
                    } else {
                        if let Some(path) = &source.path {
                            s.memory.stale_path(path, source.hash.as_deref());
                        }
                        let id = source_id.clone();
                        s.sources.insert(id.clone(), source);
                        id
                    };
                    source_ids.insert(source_id, id);
                }
                remap_source_ids(&mut result, &source_ids);
                // Temporary history IDs cannot escape into the owning session.
                if result["truncated"] == true
                    && let Some(id) = result["archive_id"]
                        .as_u64()
                        .or_else(|| result["next_cursor"]["id"].as_u64())
                    && let Ok(bundle) = temp.history.read(id)
                {
                    // The archive was created in a temporary read session.
                    // Its result can contain source objects whose IDs were
                    // allocated there, so remap the archived messages too;
                    // otherwise a later history continuation exposes IDs that
                    // the owning session cannot resolve.
                    let mut messages = bundle.messages.clone();
                    for message in &mut messages {
                        remap_source_ids(message, &source_ids);
                    }
                    let id = s.history.push(messages, true);
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
    s.task.migrate_legacy_plan();
    s.status = "running".into();
    s.last_error = None;
    let started = Instant::now();
    let initial_tokens = s.input_tokens.saturating_add(s.output_tokens);
    let mut failure = None;
    let mut finalization_attempts = s.progress_recovery.finalization_attempts;
    let mut length_recoveries = 0usize;
    let mut review_response_failures = 0usize;
    let mut tool_failures = tools::recovery::FailureTracker::default();
    let mut repetitions = std::collections::BTreeMap::<String, usize>::new();
    let mut rounds_without_progress = s.progress_recovery.rounds_without_progress.max(
        s.run_guidance["progress_recovery"]["rounds_without_progress"]
            .as_u64()
            .unwrap_or(0) as usize,
    );
    let mut repeated_read_detected = s.progress_recovery.repeated_read
        || s.run_guidance["progress_recovery"]["repeated_read"]
            .as_bool()
            .unwrap_or(false);
    let mut verified_count = s
        .investigations
        .iter()
        .filter(|i| i.status == "verified")
        .count();
    if let Err(e) = s.config.runnable() {
        failure = Some(e.to_string());
    }
    if s.input_tokens
        .checked_add(s.output_tokens)
        .is_none_or(|total| total == usize::MAX)
    {
        failure = Some(
            "token_counter_exhausted: reported usage exceeds supported range; start a new session"
                .into(),
        );
    }
    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
    while failure.is_none() && !cancel.is_cancelled() {
        while let Ok(command) = commands.try_recv() {
            match command {
                RunCommand::Configure(config) => s.pending_config = Some(*config),
                RunCommand::Tools(names) => {
                    // The web layer validates against its owner snapshot, but
                    // this private session may have advanced before the
                    // command is consumed. Preserve any workflow tools that
                    // became mandatory in that interval.
                    s.active_tools = ToolRegistry::normalize_tool_selection(&s, &names);
                    // An explicit web selection supersedes a model's older
                    // tool_select request that was waiting for the next batch.
                    s.pending_tools = None;
                }
            }
        }
        if let Some(config) = s.pending_config.clone() {
            // Try history cleanup and the new limits on a private candidate.
            // A rejected setting must not leave an irreversible history prune
            // behind while the old configuration remains active.
            let mut candidate = s.clone();
            let result = candidate
                .history
                .prune(config.history_bytes)
                .and_then(|_| apply_config(&mut candidate, config));
            match result {
                Ok(()) => {
                    candidate.pending_config = None;
                    s = candidate;
                    emit(
                        &events,
                        AgentEvent::Notice {
                            session: s.id.clone(),
                            text: "Settings applied at request boundary".into(),
                        },
                        &cancel,
                        run_deadline(started, &s.config),
                    )
                    .await;
                }
                Err(error) => {
                    emit(
                        &events,
                        AgentEvent::Notice {
                            session: s.id.clone(),
                            text: format!("Settings pending cleanup: {error}"),
                        },
                        &cancel,
                        run_deadline(started, &s.config),
                    )
                    .await;
                }
            }
        }
        if started.elapsed().as_secs() >= s.config.run_timeout_secs
            || s.input_tokens
                .saturating_add(s.output_tokens)
                .saturating_sub(initial_tokens)
                >= s.config.run_tokens
        {
            failure = Some("run_budget_exhausted: partial results and memory retained".into());
            break;
        }
        if !s.config.source_document_review {
            s.document_review.pending = false;
        }
        let spent = s
            .input_tokens
            .saturating_add(s.output_tokens)
            .saturating_sub(initial_tokens);
        let remaining = s.config.run_tokens.saturating_sub(spent);
        let seconds_remaining = s
            .config
            .run_timeout_secs
            .saturating_sub(started.elapsed().as_secs());
        let fraction = (remaining as f64 / s.config.run_tokens as f64)
            .min(seconds_remaining as f64 / s.config.run_timeout_secs as f64);
        let budget_phase =
            if finalization_attempts > 0 || fraction <= s.config.verification_reserve_ratio {
                "verify"
            } else if fraction <= s.config.writing_reserve_ratio {
                "draft"
            } else {
                "investigate"
            };
        // Progress survives resume and budget increases; only a new task resets it.
        let rank = |phase: &str| match phase {
            "answer" => 3,
            "verify" => 2,
            "draft" => 1,
            _ => 0,
        };
        let mut phase = budget_phase.to_string();
        if rank(&s.task.phase) > rank(&phase) {
            phase = s.task.phase.clone();
        }
        // Chat explanations get a bounded investigation hint, not a forced finish:
        // missing evidence may still be read, while document workflows retain verification.
        if s.task_rounds >= 6
            && !s.task.require_investigation
            && s.task.workflow != "source_document"
            && s.task.current_todo().is_none()
            && s.task.deliverables.is_empty()
            && s.investigations.is_empty()
            && !s.document_written
        {
            phase = "answer".into();
        }
        if s.task.require_investigation
            && !s.document_written
            && s.task_rounds >= 6
            && phase != "verify"
        {
            phase = "draft".into();
        }
        if finalization_attempts > 0
            || !s.document_review.issues.is_empty()
            || s.completion_review.checks.iter().any(|c| c.status != "met")
        {
            // A rejected document completion always returns to verification,
            // even if the model previously declared itself ready to answer.
            phase = "verify".into();
        }
        let document_work = s.is_document_work();
        let planned_work = s.task.current_todo().is_some();
        let focused_repair = s.completion_review.stalled_reviews
            >= COMPLETION_REVIEW_NO_PROGRESS_LIMIT
            || s.completion_review.repair_rounds >= COMPLETION_REPAIR_ROUND_LIMIT
            || s.document_review.stalled_attempts >= s.config.review_limit
            || s.progress_recovery.recovery_reason.is_some();
        let repeated_outcome_focus =
            s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit;
        let substantive_focus = s.progress_recovery.rounds_without_substantive_progress
            >= artifact_focus_limit(&s.config);
        let artifact_focus =
            s.progress_recovery.artifact_edits_without_milestone >= artifact_focus_limit(&s.config);
        let progress_recovery = s.checkpoint.is_none()
            && (repeated_read_detected
                || rounds_without_progress >= s.config.stall_round_limit
                || focused_repair
                || repeated_outcome_focus
                || substantive_focus
                || artifact_focus);
        if progress_recovery {
            phase = if document_work {
                if phase == "verify" || s.document_written || !s.investigations.is_empty() {
                    "verify"
                } else {
                    "draft"
                }
            } else if !planned_work {
                "answer"
            } else {
                phase.as_str()
            }
            .into();
        }
        s.task.phase = phase.clone();
        let focused_instruction = "Focused recovery: choose the first document_review issue, unmet completion check or current to-do and perform one concrete action that changes the requested result or verifies specific missing evidence. Read recovery_reason and the last tool's recovery contract; correct the cause or choose a different action before retrying. A task_plan applied=false or unchanged=true result did no work. Do not submit another final answer with unfinished work, cycle between earlier file versions, merely rewrite the plan, or save another summary. After a real edit, advance its to-do or verify the resulting section. If the original result already exists, verify it with the relevant tool, then complete only the actual remaining work. Document retry counts are recovery signals, not permission to stop or weaken requirements: continue to final verification within the remaining tokens and time.";
        s.run_guidance = json!({"task_rounds":s.task_rounds,"finalization_attempts":finalization_attempts,"phase":phase,"remaining_tokens":remaining,"remaining_seconds":seconds_remaining,
            "recovery_reason":s.progress_recovery.recovery_reason,"action_required":s.progress_recovery.action_required,
            "progress_recovery":{"active":progress_recovery,"focused":focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus,"rounds_without_progress":rounds_without_progress,"rounds_without_substantive_progress":s.progress_recovery.rounds_without_substantive_progress,"repeated_outcome_rounds":s.progress_recovery.repeated_outcome_rounds,"artifact_edits_without_milestone":s.progress_recovery.artifact_edits_without_milestone,"repeated_read":repeated_read_detected},
            "current_todo":s.task.current_todo(),
            "max_tool_calls":32.min(if s.checkpoint.is_some() { ContextManager::cleanup_result_budget(&s.config) } else { s.config.batch_tokens } / 200),
            "plan_pending_count":s.task.todos.iter().filter(|item| !item.done).count(),
            "plan_instruction":"Execute current_todo before later items. Insert a concrete prerequisite before it when needed, or split a broad pending item into ordered smaller outcomes while preserving its goal. Complete the current item through task_plan with the observed result; plan edits do not reset no-progress recovery. If the plan is full, finish the current item or remove obsolete pending items; do not stop the task.",
            "writing_reserve_tokens":(s.config.run_tokens as f64*s.config.writing_reserve_ratio) as usize,
            "verification_reserve_tokens":(s.config.run_tokens as f64*s.config.verification_reserve_ratio) as usize,
            "document_repair_limit":s.config.document_repair_limit,
            "document_repair_requests_used":s.document_review.repair_requests,
            "document_repair_requests_remaining":s.config.document_repair_limit.saturating_sub(s.document_review.repair_requests),
            "pending_count":s.investigations.iter().filter(|i|i.status != "verified").count(),
            "completion_error":if finalization_attempts > 0 || s.last_error.as_deref().is_some_and(|error| error.starts_with("task_plan_pending:") || error.starts_with("completion_review") || error.starts_with("documentation_coverage")) { s.last_error.as_deref() } else { None },
            "instruction":if focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus { focused_instruction } else if progress_recovery { if document_work { "Progress recovery: repeated investigation has not changed the document or verified an item. Use the evidence already gathered to make one small, safe document_edit now, or verify an existing written item. Inspect the output hash if needed. Read only a specific missing source range that directly blocks that action. Do not gather more general evidence or save another memory first. If a claim cannot be supported, mark that gap in the relevant section and continue with supported work; do not invent evidence. A checkpoint remains the only exception for memory maintenance." } else if planned_work { "Progress recovery: plan edits or repeated reads have not produced an outcome. Execute the first unfinished item using available evidence and tools. Do not recreate the plan or save another summary. Insert only a concrete missing prerequisite; complete an item only with the actual result. If evidence is missing, read only the necessary range." } else { "Progress recovery: repeated preparation has not produced an outcome. Correct any necessary task_plan call using its returned example, then carry out the first concrete action; otherwise answer from existing evidence. Do not repeat an unchanged call or save another summary." } } else { match phase.as_str() {"answer"=>"Answer the user now from gathered evidence. Read further only for a concrete missing fact required by the question. Do not save memory before answering a simple explanation. State any missing coverage instead of claiming exhaustive review.","verify"=>"Stop expanding scope. For source documentation, batch targeted reads for missing evidence, then repair known issues in their original locations with targeted section or text edits when safe. Review findings are edit instructions, not document content: do not append a review, checks, improvements, or TODO section unless the user explicitly requested it. If a fact remains unverified, qualify it where the relevant claim appears; include a limitation only when needed for the requested document. Inspect the final outline for review-note headings before completion. Use document_edit_batch for related edits from one document snapshot; its operations are applied in order. Run verify_batch once after all edits, not after every small correction. Only requests containing document_edit or document_edit_batch advance the review interval (one per request, including failed edits); reads and verification do not. The runtime reviews after an executed edit batch reaches the interval, preserving all sibling calls. Unchanged rejected documents reuse their findings. This interval is not a total edit allowance: keep correcting the original requirements within the remaining run tokens and time. For questions or existing-document summaries, answer from the content already read and report any missing coverage.","draft"=>"For source documentation, save each investigated section in a separate edit as soon as its evidence is ready. Check the current outline; use insert_before/insert_after for siblings and insert_first_child/insert_last_child for nested sections when that preserves the document flow. Copy section_path when headings repeat; preserve verification budget. For questions or existing-document summaries, finish the chat answer using targeted reads only.",_=>"For source documentation, investigate one section, save it, then move to the next. Create only a short opening with the first ready section; inspect the outline before each later addition, copy section_path when headings repeat, and use sibling or child insertion to place it within the hierarchy. For a SOURCE CODE question, the first batch should locate the requested symbols/routes with source_search or code_outline scoped to the named files. Batch independent searches or reads together instead of paying a model round per file. Do not begin with file_read of each file from line 1; that often misses the target and requires another read. After locating the branch, file_read only its relevant range with explicit start_line and max_lines, or use symbol_read. For an existing-document summary, read the relevant document sections directly. Answer once evidence is sufficient; no source audit or document write is required."} }});
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
            if !document_work
                && (cp.attempts >= context::CHECKPOINT_MAX_REQUESTS
                    || cp.failed_attempts >= context::CHECKPOINT_MAX_FAILURES)
            {
                failure = Some(format!(
                    "checkpoint_retry_limit: {} requests, {} failed requests; last cause: {}; original context retained",
                    cp.attempts,
                    cp.failed_attempts,
                    cp.last_failure
                        .as_deref()
                        .unwrap_or("checkpoint_complete was not called successfully")
                ));
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
        let reviewing_completion = s.completion_review.pending && s.checkpoint.is_none();
        let reviewing_document =
            !reviewing_completion && s.document_review.pending && s.checkpoint.is_none();
        let reviewing_answer =
            !reviewing_completion && s.answer_draft.is_some() && s.checkpoint.is_none();
        if reviewing_completion {
            request = match tools::completion_review::request(&mut s) {
                Ok(request) => request,
                Err(error) => {
                    if recover_review_setup(&mut s, &error.to_string()) {
                        continue;
                    }
                    s.status = "partial".into();
                    s.last_error = Some(error.to_string());
                    break;
                }
            };
        }
        if reviewing_document {
            request = match tools::document_review::request(&mut s) {
                Ok(request) => request,
                Err(error) => {
                    if recover_review_setup(&mut s, &error.to_string()) {
                        continue;
                    }
                    failure = Some(error.to_string());
                    break;
                }
            };
        }
        if reviewing_answer {
            request = match tools::answer_review::request(&s) {
                Ok(request) => request,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            };
        }
        if document_work
            && s.progress_recovery.action_required
            && s.checkpoint.is_none()
            && !reviewing_completion
            && !reviewing_document
            && !reviewing_answer
        {
            // A rejected final must return to a concrete tool action. Review
            // requests remain tool-free, and a subsequent final is still gated.
            request["tool_choice"] = json!("required");
        }
        let buffer_answer = reviewing_completion
            || reviewing_document
            || reviewing_answer
            || (s.checkpoint.is_none() && tools::answer_review::eligible(&s));
        let request_tokens = context::count(&request, &s.config.model);
        // Reasoning models spend output tokens on reasoning too; retain the
        // configured output allowance and reserve it for every cleanup round.
        let mut request_config = s.config.clone();
        if reviewing_document {
            // A short first response keeps ordinary review calls bounded. If
            // the provider exhausts that allowance before emitting JSON, give
            // the bounded recovery request the configured output allowance so
            // reasoning tokens cannot starve the verdict itself.
            if review_response_failures == 0 {
                request_config.output_tokens = request_config.output_tokens.min(4096);
            }
        }
        if request_tokens
            .saturating_add(request_config.output_tokens)
            .saturating_add(512)
            > s.config.context_tokens
        {
            failure = Some(format!(
                "context_limit: input estimate {request_tokens} + output {} + margin 512 exceeds {}; original context retained",
                request_config.output_tokens, s.config.context_tokens
            ));
            break;
        }
        if s.input_tokens
            .saturating_add(s.output_tokens)
            .saturating_sub(initial_tokens)
            .saturating_add(request_tokens)
            .saturating_add(request_config.output_tokens)
            > s.config.run_tokens
        {
            failure = Some("run_budget_exhausted: insufficient budget for next request".into());
            break;
        }
        s.task_rounds += 1;
        s.activity = json!({"stage":if reviewing_completion {"completion_review"} else if reviewing_document {"document_review"} else if reviewing_answer {"answer_review"} else {"model"},"started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.task_rounds});
        snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
        let deadline = run_deadline(started, &s.config);
        if tokio::time::Instant::now() >= deadline {
            failure = Some("run_timeout: event delivery exhausted execution deadline".into());
            break;
        }
        if let Some(cp) = &mut s.checkpoint {
            cp.attempts += 1;
        }
        let document_workflow = s.is_document_work() || tools::completion_review::required(&s);
        request[crate::llm::STREAM_DELTAS_MARKER] = json!(!(buffer_answer || document_workflow));
        let (tx, mut rx) = mpsc::channel(64);
        let event_tx = events.clone();
        let sid = s.id.clone();
        let relay_done = CancellationToken::new();
        let done = relay_done.clone();
        let relay_cancel = cancel.clone();
        let relay = tokio::spawn(async move {
            loop {
                let text = tokio::select! {
                    biased;
                    _ = relay_cancel.cancelled() => break,
                    _ = done.cancelled(), if !rx.is_closed() => { rx.close(); continue; },
                    text = rx.recv() => text,
                };
                let Some(text) = text else { break };
                if buffer_answer || document_workflow {
                    continue;
                }
                emit(
                    &event_tx,
                    AgentEvent::Delta {
                        session: sid.clone(),
                        text,
                    },
                    &relay_cancel,
                    deadline,
                )
                .await;
            }
        });
        let _relay_guard = AbortOnDrop(relay.abort_handle());
        let usage_before = (s.input_tokens, s.output_tokens);
        let response = tokio::select! {_ = cancel.cancelled()=>Err(anyhow::anyhow!("cancelled")),result=tokio::time::timeout_at(deadline,std::panic::AssertUnwindSafe(client.complete(request,&request_config,cancel.clone(),tx)).catch_unwind())=>match result{Ok(Ok(r))=>r,Ok(Err(_))=>Err(anyhow::anyhow!("model_worker_panic: model request interrupted; session retained")),Err(_)=>Err(anyhow::anyhow!("run_timeout"))}};
        relay_done.cancel();
        let _ = relay.await;
        let mut completion = match response {
            Ok(r) => r,
            Err(e) => {
                s.usage_incomplete = true;
                let attempts = e
                    .downcast_ref::<crate::llm::CompletionError>()
                    .map_or(1, crate::llm::CompletionError::attempts);
                s.input_tokens = s
                    .input_tokens
                    .saturating_add(request_tokens.saturating_mul(attempts));
                if recover_unexecuted_batch(&mut s, &e.to_string()) {
                    // Parsing failures have no trusted usage receipt. Charge
                    // the reserved output as well as every attempted input so
                    // malformed responses cannot escape the run token budget.
                    s.output_tokens = s
                        .output_tokens
                        .saturating_add(request_config.output_tokens.saturating_mul(attempts));
                    continue;
                }
                failure = Some(e.to_string());
                break;
            }
        };
        if let Err(error) = crate::llm::validate_completion_bounds(&completion) {
            let extra_attempts = completion.attempts.saturating_sub(1);
            if extra_attempts > 0 {
                s.usage_incomplete = true;
            }
            s.input_tokens = s
                .input_tokens
                .saturating_add(request_tokens.saturating_mul(extra_attempts));
            if let Some(usage) = completion.usage {
                s.input_tokens = s.input_tokens.saturating_add(usage.input);
                s.output_tokens = s.output_tokens.saturating_add(usage.output);
                if let Some(cached) = usage.cached {
                    s.cached_tokens = Some(s.cached_tokens.unwrap_or(0).saturating_add(cached));
                }
            } else {
                s.usage_incomplete = true;
                s.input_tokens = s.input_tokens.saturating_add(request_tokens);
                s.output_tokens = s.output_tokens.saturating_add(request_config.output_tokens);
            }
            if recover_unexecuted_batch(&mut s, &error.to_string()) {
                continue;
            }
            failure = Some(error.to_string());
            break;
        }
        if completion.attempts > 1 {
            s.usage_incomplete = true;
            s.input_tokens = s
                .input_tokens
                .saturating_add(request_tokens.saturating_mul(completion.attempts - 1));
        }
        if let Some(u) = completion.usage {
            s.input_tokens = s.input_tokens.saturating_add(u.input);
            s.output_tokens = s.output_tokens.saturating_add(u.output);
            if let Some(c) = u.cached {
                s.cached_tokens = Some(s.cached_tokens.unwrap_or(0).saturating_add(c));
            }
        } else {
            s.usage_incomplete = true;
            s.input_tokens = s.input_tokens.saturating_add(request_tokens);
            s.output_tokens = s
                .output_tokens
                .saturating_add(if completion.length_limited {
                    // No provider usage: length exhaustion may be invisible reasoning.
                    request_config.output_tokens
                } else {
                    context::tokens(&completion.text, &s.config.model)
                        + completion
                            .calls
                            .iter()
                            .map(|c| context::tokens(&c.arguments, &s.config.model))
                            .sum::<usize>()
                });
        }
        if reviewing_completion {
            s.completion_review.input_tokens = s
                .completion_review
                .input_tokens
                .saturating_add(s.input_tokens.saturating_sub(usage_before.0));
            s.completion_review.output_tokens = s
                .completion_review
                .output_tokens
                .saturating_add(s.output_tokens.saturating_sub(usage_before.1));
            // As with document review, a complete validated JSON verdict can
            // survive a provider's spurious length flag. Partial JSON cannot.
            let result = if completion.discarded_tool_calls || !completion.calls.is_empty() {
                Err(anyhow::anyhow!(
                    "completion_review_invalid: return complete JSON without tool calls"
                ))
            } else {
                tools::completion_review::finish(&mut s, &completion.text)
            };
            match result {
                Err(error) => {
                    let reason = error.to_string();
                    if s.is_document_work() && !reason.starts_with("completion_review_invalid:") {
                        if recover_review_setup(&mut s, &reason) {
                            continue;
                        }
                        failure = Some(reason);
                        break;
                    }
                    review_response_failures += 1;
                    s.last_error = Some(reason);
                    if review_response_failures >= REVIEW_RESPONSE_LIMIT && !s.is_document_work() {
                        s.status = "partial".into();
                        break;
                    }
                    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                    continue;
                }
                Ok(None) => {
                    review_response_failures = 0;
                    if !s.completion_review.pending {
                        let met = s
                            .completion_review
                            .checks
                            .iter()
                            .filter(|check| check.status == "met")
                            .count();
                        if met > s.completion_review.best_met {
                            s.completion_review.best_met = met;
                            s.completion_review.stalled_reviews = 0;
                            s.completion_review.repair_rounds = 0;
                            s.progress_recovery.repeated_outcome_rounds = 0;
                            s.progress_recovery.rounds_without_progress = 0;
                            s.progress_recovery.rounds_without_substantive_progress = 0;
                            s.progress_recovery.artifact_edits_without_milestone = 0;
                            s.progress_recovery.repeated_read = false;
                            rounds_without_progress = 0;
                            repeated_read_detected = false;
                            repetitions.clear();
                            finalization_attempts = 0;
                            s.progress_recovery.finalization_attempts = 0;
                        }
                        s.completion_review.stalled_reviews =
                            s.completion_review.stalled_reviews.saturating_add(1);
                        tools::completion_review::schedule_repairs(&mut s);
                        if s.completion_review.stalled_reviews >= COMPLETION_REVIEW_EXHAUST_LIMIT
                            && !recover_document(
                                &mut s,
                                "Completion checks still fail. Follow the first unmet check using different evidence or a concrete correction.",
                            )
                        {
                            s.status = "partial".into();
                            s.last_error = Some("completion_review_no_progress: focused repairs did not satisfy another criterion; repair tasks and unmet checks retained for a changed approach".into());
                            break;
                        }
                        if s.is_document_work() {
                            s.progress_recovery.action_required = true;
                        }
                        emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"완료 조건에 미충족 또는 확인 불가 항목이 있어 보완 작업을 이어갑니다.".into() }, &cancel, run_deadline(started, &s.config)).await;
                    }
                    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                    continue;
                }
                Ok(Some(answer)) => {
                    review_response_failures = 0;
                    completion.text = answer;
                    completion.length_limited = false;
                    s.last_error = None;
                }
            }
        }
        if reviewing_document {
            s.document_review.input_tokens += s.input_tokens.saturating_sub(usage_before.0);
            s.document_review.output_tokens += s.output_tokens.saturating_sub(usage_before.1);
            // Validate the visible response before looking at provider
            // metadata. Some compatible providers attach a spurious tool call
            // or report finish_reason=length even when the JSON object is
            // complete. The review request has no executable tools, so a
            // complete, hash-checked verdict is safe to accept in that case.
            let review_result = if completion.discarded_tool_calls
                || !completion.calls.is_empty()
                || completion.text.trim().is_empty()
            {
                Err(anyhow::anyhow!(
                    "document_review_incomplete: review must return complete JSON without tools"
                ))
            } else {
                match tools::document_review::finish(&mut s, &completion.text) {
                    Ok(()) => Ok(()),
                    Err(error)
                        if (completion.length_limited || !completion.calls.is_empty())
                            && error.to_string().starts_with("document_review_invalid:") =>
                    {
                        Err(anyhow::anyhow!(
                            "document_review_incomplete: review must return complete JSON without tools"
                        ))
                    }
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = review_result {
                let reason = error.to_string();
                review_response_failures += 1;
                s.last_error = Some(reason.clone());
                if (s.is_document_work() || review_response_failures < REVIEW_RESPONSE_LIMIT)
                    && (reason.starts_with("document_review_invalid:")
                        || reason.starts_with("document_review_incomplete:")
                        || reason.starts_with("document_review_stale:"))
                {
                    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                    continue;
                }
                if recover_review_setup(&mut s, &reason) {
                    continue;
                }
                // Document retries retain the pending page until the run budget.
                // Other workflows retain the bounded protocol-recovery policy.
                if reason.starts_with("document_review_invalid:")
                    || reason.starts_with("document_review_incomplete:")
                    || reason.starts_with("document_review_stale:")
                {
                    s.status = "partial".into();
                    s.document_review.pending = true;
                } else {
                    failure = Some(reason);
                }
                break;
            }
            review_response_failures = 0;
            if !s.document_review.pending && !s.document_review.issues.is_empty() {
                s.last_error = Some(format!(
                    "document_review: {}",
                    s.document_review.issues.join("; ")
                ));
                s.progress_recovery.action_required = true;
                if s.document_review.stalled_attempts >= s.config.review_limit {
                    recover_document(
                        &mut s,
                        "Document review still has unresolved findings. Correct the first finding in its original section, then verify that section; do not request another unchanged review.",
                    );
                }
            }
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            continue;
        }
        if reviewing_answer {
            s.answer_review_input_tokens += s.input_tokens.saturating_sub(usage_before.0);
            s.answer_review_output_tokens += s.output_tokens.saturating_sub(usage_before.1);
        }
        if reviewing_answer
            && (completion.length_limited
                || !completion.calls.is_empty()
                || completion.text.trim().is_empty())
        {
            let reason = "answer_review_incomplete: review must return one complete answer without tools; draft retained";
            review_response_failures += 1;
            s.last_error = Some(reason.into());
            if review_response_failures < REVIEW_RESPONSE_LIMIT {
                snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                continue;
            }
            failure = Some(reason.into());
            break;
        }
        if reviewing_answer {
            review_response_failures = 0;
        }

        if !completion.length_limited
            && completion.calls.is_empty()
            && completion.text.trim().is_empty()
        {
            if recover_document(
                &mut s,
                "The model returned an empty response. Use a concrete document repair or verification tool before the final answer.",
            ) {
                s.progress_recovery.action_required = true;
                continue;
            }
            // A provider may legally return stop with an empty content field.
            // Treating that as a successful final answer would mark the task
            // complete while persisting an empty assistant message.
            failure = Some(
                "empty_completion: model returned neither text nor tool calls; resume to retry"
                    .into(),
            );
            break;
        }

        if completion.length_limited && buffer_answer && !completion.discarded_tool_calls {
            // A source draft need not be streamed or continued verbatim: the
            // bounded review can produce a complete answer from its evidence.
            s.answer_draft = Some(completion.text.clone());
            continue;
        }
        if completion.length_limited {
            let mut partial = assistant(&completion.text, &[]);
            partial["partial"] = json!(true);
            partial["continues_previous"] =
                json!(s.continuation.is_some() && s.checkpoint.is_none());
            let id = s.history.push(vec![partial], true);
            if let Some(cp) = &mut s.checkpoint {
                cp.maintenance_bundle_ids.push(id);
            } else {
                s.continuation = Some(completion.discarded_tool_calls);
            }
            length_recoveries += 1;
            s.activity = json!({"stage":"continuing","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.task_rounds});
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            if length_recoveries >= LENGTH_RECOVERY_LIMIT {
                let reason = format!(
                    "length_recovery_limit: {length_recoveries} consecutive output-limit responses. Split document edits into smaller complete calls and keep final reports concise; discarded tool batches did not execute."
                );
                if !recover_document(&mut s, &reason) {
                    failure = Some(format!(
                        "length_recovery_limit: partial text retained after {LENGTH_RECOVERY_LIMIT} output-limit responses; shorten the requested answer or adjust output/reasoning settings before resuming"
                    ));
                    break;
                }
            }
            // Use the normal next-request path for budget, timeout, cancellation
            // and checkpoint checks; never execute a length-limited tool batch.
            continue;
        }
        // Independent later truncations get their own bounded recovery window.
        length_recoveries = 0;
        let continuing = s.checkpoint.is_none()
            && (s.continuation.is_some()
                || (reviewing_completion && s.completion_review.continues_previous));
        if s.checkpoint.is_none() {
            s.continuation = None;
        }
        let batch_limit = if s.checkpoint.is_some() {
            ContextManager::cleanup_result_budget(&s.config)
        } else {
            s.config.batch_tokens
        };
        let call_limit = 32.min(batch_limit / 200);
        if completion.calls.len() > call_limit {
            let reason =
                format!("tool_call_batch_limit: maximum {call_limit} calls at this result budget");
            if recover_unexecuted_batch(&mut s, &reason) {
                continue;
            }
            failure = Some(reason);
            break;
        }
        if completion.calls.is_empty() {
            if !reviewing_answer && s.checkpoint.is_none() && tools::answer_review::eligible(&s) {
                s.answer_draft = Some(completion.text.clone());
                s.activity = json!({"stage":"answer_review"});
                emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"Checking the source answer against delivered evidence (one bounded review).".into() }, &cancel, run_deadline(started, &s.config)).await;
                snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                continue;
            }
            let citation_issues = if reviewing_answer {
                s.answer_reviewed = true;
                s.answer_review_original = s.answer_draft.take();
                tools::answer_review::citation_issues(&s, &completion.text)
            } else if s.answer_reviewed {
                tools::answer_review::citation_issues(&s, &completion.text)
            } else {
                vec![]
            };
            s.answer_review_issues = citation_issues.clone();
            let hold_document_final = document_workflow && s.checkpoint.is_none();
            if buffer_answer && (!hold_document_final || !citation_issues.is_empty()) {
                emit(
                    &events,
                    AgentEvent::Delta {
                        session: s.id.clone(),
                        text: completion.text.clone(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
            }
            let mut message = assistant(&completion.text, &[]);
            if continuing {
                message["continues_previous"] = json!(true);
            }
            // Do not expose an unverified "done" claim through history snapshots.
            let id = if hold_document_final {
                0
            } else {
                s.history.push(vec![message.clone()], true)
            };
            if let Some(cp) = &mut s.checkpoint {
                cp.maintenance_bundle_ids.push(id);
                continue;
            }
            if s.pending_config.is_some() {
                failure =
                    Some("Pending settings still require cleanup; old settings retained".into());
                break;
            }
            if !citation_issues.is_empty() {
                if hold_document_final {
                    message["partial"] = json!(true);
                    s.history.push(vec![message], true);
                }
                s.status = "partial".into();
                s.last_error = Some(format!(
                    "answer_citation_check: {}; one review completed, further review is not automatic",
                    citation_issues.join("; ")
                ));
                break;
            }
            if let Some(item) = s.task.current_todo() {
                s.last_error = Some(format!(
                    "task_plan_pending: {} ({}) is unfinished. Continue its actual work, or complete it with the observed result using task_plan if it is already done. Do not repeat completed investigation just to update the plan.",
                    item.id, item.text
                ));
                // A premature answer is a request to continue the plan, not a
                // failed evidence review. Do not spend the finalization retry
                // allowance or stop the task for unfinished plan bookkeeping.
                rounds_without_progress = rounds_without_progress.saturating_add(1);
                s.progress_recovery.rounds_without_progress = rounds_without_progress;
                s.progress_recovery.repeated_outcome_rounds = s
                    .progress_recovery
                    .repeated_outcome_rounds
                    .saturating_add(1);
                s.run_guidance["progress_recovery"]["rounds_without_progress"] =
                    json!(rounds_without_progress);
                s.run_guidance["progress_recovery"]["repeated_outcome_rounds"] =
                    json!(s.progress_recovery.repeated_outcome_rounds);
                s.run_guidance["progress_recovery"]["active"] = json!(
                    repeated_read_detected || rounds_without_progress >= s.config.stall_round_limit
                );
                if s.is_document_work() {
                    s.progress_recovery.action_required = true;
                }
                if repeated_outcome_exhausted(&s)
                    && !recover_document(
                        &mut s,
                        "Repeated final claims left the current to-do unfinished. Complete its actual work and record the result before answering.",
                    )
                {
                    s.status = "partial".into();
                    s.last_error = Some(REPEATED_OUTCOME_ERROR.into());
                    break;
                }
                emit(
                    &events,
                    AgentEvent::Notice {
                        session: s.id.clone(),
                        text: "Continuing the first unfinished to-do item before the final answer."
                            .into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                continue;
            }
            if s.capabilities.active {
                match tools::capabilities::audit(&s, &cancel) {
                    Ok(audit) => {
                        tools::capabilities::remember(&mut s, &audit);
                        if !audit.ready {
                            if s.progress_recovery.coverage_fingerprint
                                == audit.progress_fingerprint
                            {
                                s.progress_recovery.coverage_stalls += 1;
                            } else {
                                s.progress_recovery.coverage_fingerprint =
                                    audit.progress_fingerprint.clone();
                                s.progress_recovery.coverage_stalls = 0;
                            }
                            tools::capabilities::enqueue(&mut s, &audit);
                            s.progress_recovery.action_required = true;
                            if s.progress_recovery.coverage_stalls >= FINALIZATION_RETRY_LIMIT
                                && !recover_document(
                                    &mut s,
                                    "Documentation coverage is unchanged. Resolve the first inventory gap with its source evidence and document binding.",
                                )
                            {
                                s.status = "partial".into();
                                s.last_error = Some("documentation_coverage_no_progress: unchanged gaps remain; inventory, evidence and repair tasks retained for resume".into());
                                break;
                            }
                            s.status = "running".into();
                            emit(&events, AgentEvent::Notice {session:s.id.clone(),text:"기능 목록과 문서의 누락 항목을 확인해 보완 작업을 계속합니다.".into()}, &cancel, run_deadline(started,&s.config)).await;
                            continue;
                        }
                    }
                    Err(error) => {
                        s.status = if cancel.is_cancelled() {
                            "cancelled"
                        } else {
                            "partial"
                        }
                        .into();
                        s.last_error = Some(format!("documentation_coverage_audit: {error}"));
                        break;
                    }
                }
            }
            if let Err(error) = tools::verify_document_write(&s) {
                s.status = "partial".into();
                s.last_error = Some(error.to_string());
            } else if s.task.require_investigation && s.investigations.is_empty() {
                s.status = "partial".into();
                s.last_error = Some("Required source-evidence coverage is missing: create investigation items with investigation action=upsert, then compare sources and document and verify them before finishing".into());
            } else if !s.investigations.is_empty() {
                let _ = tools::revalidate(&mut s);
                if s.investigations.iter().any(|i| i.status != "verified") {
                    s.status = "partial".into();
                    s.last_error =
                        Some("Unverified investigation items remain; document is partial".into());
                } else {
                    match tools::audit_document(&mut s) {
                        Ok(audit) if audit["structural_ok"] == true => {
                            if s.document_written
                                && s.config.source_document_review
                                && !tools::document_review::approved(&s)
                            {
                                if tools::document_review::rejected_on_current_result(&s) {
                                    s.status = "partial".into();
                                    s.last_error = Some(format!(
                                        "document_review: unchanged document still requires correction: {}",
                                        s.document_review.issues.join("; ")
                                    ));
                                } else {
                                    s.document_review.pending = true;
                                    s.status = "running".into();
                                    emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"Reviewing the document against source evidence and requested coverage.".into() }, &cancel, run_deadline(started, &s.config)).await;
                                    continue;
                                }
                            } else {
                                s.status = "complete".into();
                                s.last_error = None;
                            }
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
                s.last_error = None;
            }
            if s.status == "complete" && tools::completion_review::required(&s) {
                match tools::completion_review::begin_final(&mut s, &completion.text, continuing) {
                    Ok(tools::completion_review::Gate::Accepted) => {}
                    Ok(tools::completion_review::Gate::Review) => {
                        s.status = "running".into();
                        emit(
                            &events,
                            AgentEvent::Notice {
                                session: s.id.clone(),
                                text: "할 일 목록 이후 실제 결과와 완료 조건을 대조합니다.".into(),
                            },
                            &cancel,
                            run_deadline(started, &s.config),
                        )
                        .await;
                        continue;
                    }
                    Ok(tools::completion_review::Gate::Repair) => {
                        tools::completion_review::schedule_repairs(&mut s);
                        s.completion_review.stalled_reviews =
                            s.completion_review.stalled_reviews.saturating_add(1);
                        if s.is_document_work() {
                            s.progress_recovery.action_required = true;
                        }
                        if s.completion_review.stalled_reviews >= COMPLETION_REVIEW_EXHAUST_LIMIT
                            && !recover_document(
                                &mut s,
                                "The unchanged result still fails completion checks. Perform the repair against actual content, not plan bookkeeping or another final claim.",
                            )
                        {
                            s.status = "partial".into();
                            s.last_error = Some("completion_review_no_progress: unchanged results still fail acceptance after focused repair; repair tasks and unmet checks retained for a changed approach".into());
                            break;
                        }
                        s.status = "running".into();
                        continue;
                    }
                    Err(error) => {
                        if recover_review_setup(&mut s, &error.to_string()) {
                            continue;
                        }
                        s.status = "partial".into();
                        s.last_error = Some(error.to_string());
                        break;
                    }
                }
            }
            if s.status == "partial"
                && (s.is_document_work() || finalization_attempts < FINALIZATION_RETRY_LIMIT)
            {
                finalization_attempts = finalization_attempts.saturating_add(1);
                s.progress_recovery.finalization_attempts = finalization_attempts;
                if s.is_document_work() {
                    s.progress_recovery.action_required = true;
                    if finalization_attempts >= FINALIZATION_RETRY_LIMIT {
                        recover_document(
                            &mut s,
                            "Finalization still has missing evidence or document work. Resolve completion_error with tools; the remaining run budget is available for finishing.",
                        );
                    }
                }
                s.status = "running".into();
                emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"Completion checks failed; returning to pending evidence verification within the remaining budget".into() }, &cancel, run_deadline(started, &s.config)).await;
                continue;
            }
            if hold_document_final && s.status == "complete" {
                s.history.push(vec![message], true);
                emit(
                    &events,
                    AgentEvent::Delta {
                        session: s.id.clone(),
                        text: completion.text.clone(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
            }
            break;
        }
        if s.checkpoint.is_none() {
            s.progress_recovery.action_required = false;
        }
        // Count the batch once and review AFTER it is executed. Rejecting the
        // response at this boundary silently lost valid edits and sibling calls.
        let repair_edit_request = s.config.source_document_review
            && s.checkpoint.is_none()
            && s.document_review.repair_started_round.is_some()
            && completion
                .calls
                .iter()
                .any(|call| matches!(call.name.as_str(), "document_edit" | "document_edit_batch"));
        if repair_edit_request {
            s.document_review.repair_requests = s.document_review.repair_requests.saturating_add(1);
        }
        if (buffer_answer || document_workflow) && !completion.text.is_empty() {
            emit(
                &events,
                AgentEvent::Delta {
                    session: s.id.clone(),
                    text: completion.text.clone(),
                },
                &cancel,
                run_deadline(started, &s.config),
            )
            .await;
        }
        let mut messages = vec![assistant(&completion.text, &completion.calls)];
        // A checkpoint acknowledges the whole batch. Evaluate it after writes,
        // even if a provider emits its call first; preserve other write ordering.
        let mut execution_calls = completion.calls.clone();
        execution_calls.sort_by_key(|call| call.name == "checkpoint_complete");
        let mut i = 0;
        let mut checkpoint_failure_recorded = false;
        let mut remaining = batch_limit;
        let mut seen_call_ids = std::collections::BTreeSet::new();
        let mut batch_document_hash: Option<String> = None;
        let prior_inventory_progress = tools::capabilities::progress(&s);
        let prior_source_count = s.sources.len();
        let prior_written_count = s
            .investigations
            .iter()
            .filter(|i| i.status == "written")
            .count();
        let prior_current_todo = s.task.current_todo().map(|item| item.id.clone());
        let prior_pending_todos = s.task.todos.iter().filter(|item| !item.done).count();
        let checkpoint_batch = s.checkpoint.is_some();
        let mut novel_artifact_change = false;
        let mut artifact_milestone = false;
        let mut novel_navigation_evidence = false;
        while i < execution_calls.len() {
            let call = &execution_calls[i];
            let replayed_call = s.ledger.contains_key(&call.id);
            // A provider may replay a complete response after a transport
            // retry. Ledger entries keep the original call signature, so a
            // cached call must be looked up with its original arguments
            // before considering the in-response document hash. Rebasing a
            // replayed later edit would otherwise turn an idempotent retry
            // into a false call_id_collision.
            let (effective_call, rebased_document_call) = if replayed_call {
                (call.clone(), false)
            } else {
                rebase_document_call(call, batch_document_hash.as_deref())
            };
            let malformed_call = call.id.trim().is_empty() || call.name.trim().is_empty();
            let duplicate_call_id = seen_call_ids.contains(&call.id);
            let parallel = ["file_list", "file_read", "source_search"]
                .contains(&call.name.as_str())
                && s.active_tools.contains(&call.name)
                && s.checkpoint.is_none()
                && s.run_guidance["phase"] != "verify"
                // Parallel workers use isolated temporary sessions. A call
                // already in the owner ledger must go through execute_one so
                // its cached result (or call-id collision) is honored instead
                // of silently running the same call again.
                && !s.ledger.contains_key(&call.id)
                && !malformed_call
                && !seen_call_ids.contains(&call.id);
            let mut group = 1;
            if parallel {
                let mut group_call_ids = std::collections::BTreeSet::new();
                group_call_ids.insert(call.id.clone());
                while i + group < execution_calls.len()
                    && ["file_list", "file_read", "source_search"]
                        .contains(&execution_calls[i + group].name.as_str())
                    && s.active_tools.contains(&execution_calls[i + group].name)
                    && !s.ledger.contains_key(&execution_calls[i + group].id)
                    && !execution_calls[i + group].id.trim().is_empty()
                    && !execution_calls[i + group].name.trim().is_empty()
                    && !seen_call_ids.contains(&execution_calls[i + group].id)
                    && !group_call_ids.contains(&execution_calls[i + group].id)
                {
                    group_call_ids.insert(execution_calls[i + group].id.clone());
                    group += 1
                }
            }
            s.activity = json!({"stage":"tools","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.task_rounds,"tools":execution_calls[i..i+group].iter().map(|c| c.name.clone()).collect::<Vec<_>>()});
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            let results = if malformed_call {
                vec![tools::envelope(Err(anyhow::anyhow!(
                    "malformed_tool_call: tool call id and name must be non-empty"
                )))]
            } else if duplicate_call_id {
                vec![tools::envelope(Err(anyhow::anyhow!(
                    "call_id_collision: duplicate tool call id in one response; call IDs must be unique"
                )))]
            } else if failure.is_some() {
                (0..group)
                    .map(|_| {
                        tools::envelope(Err(anyhow::anyhow!(
                            "tool_batch_aborted: earlier terminal failure; call not executed"
                        )))
                    })
                    .collect()
            } else if cancel.is_cancelled()
                || tokio::time::Instant::now() >= run_deadline(started, &s.config)
            {
                let reason = if cancel.is_cancelled() {
                    "cancelled: tool not started"
                } else {
                    let reason = "run_timeout: tool not started after execution deadline";
                    failure = Some(reason.into());
                    reason
                };
                (0..group)
                    .map(|_| tools::envelope(Err(anyhow::anyhow!(reason))))
                    .collect()
            } else if parallel {
                read_parallel(&mut s, &execution_calls[i..i + group], &cancel).await
            } else {
                let (next, result) = execute_one(s, effective_call, &cancel).await;
                s = next;
                // The tool ran with a rebased execution argument, but the
                // provider's call ID identifies the original assistant call.
                // Keep that original signature in the idempotency ledger so a
                // replay of the same response is served from the cache rather
                // than reported as a call-id collision.
                if rebased_document_call && result["status"] == "ok" {
                    s.ledger.insert(
                        call.id.clone(),
                        (format!("{}:{}", call.name, call.arguments), result.clone()),
                    );
                }
                vec![result]
            };
            for (call, mut result) in execution_calls[i..i + group].iter().zip(results) {
                if matches!(
                    call.name.as_str(),
                    "file_edit" | "file_write" | "file_patch"
                ) && !replayed_call
                    && result["status"] == "ok"
                    && let Some(files) = result["data"]["files"].as_array()
                {
                    for file in files.iter().filter(|file| file["changed"] == true) {
                        if let Some(path) = file["path"].as_str() {
                            let digest = file["hash"].as_str().unwrap_or("<deleted>");
                            if file["exists"] == true {
                                artifact_milestone |=
                                    s.progress_recovery.remember_artifact_path(path.into());
                            }
                            novel_artifact_change |= s
                                .progress_recovery
                                .remember_artifact(path.into(), digest.into());
                        }
                    }
                }
                if call.name == "db_execute"
                    && !replayed_call
                    && result["status"] == "ok"
                    && result["data"]["committed"] == true
                {
                    novel_artifact_change |= s.progress_recovery.remember_artifact(
                        "db_execute".into(),
                        tools::hash(call.arguments.as_bytes()),
                    );
                }
                let cache_parallel_result = parallel && result["status"] == "ok";
                tools::recovery::attach(&s, call, &mut result);
                if let Some(reason) =
                    tool_failures.observe(&call.name, &result, s.config.stall_round_limit)
                    && failure.is_none()
                {
                    let correctable = tools::recovery::correctable_document_error(&result);
                    if !correctable || !recover_document(&mut s, &reason) {
                        failure = Some(reason);
                    }
                }
                if result["error"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("tool_worker_panic"))
                {
                    failure = result["error"].as_str().map(str::to_owned);
                }
                if rebased_document_call && result["status"] == "ok" {
                    result["data"]["batch_rebased"] = json!(true);
                }
                if matches!(call.name.as_str(), "document_edit" | "document_edit_batch")
                    && result["status"] == "ok"
                    && let Some(digest) = result["data"]["hash"].as_str()
                {
                    batch_document_hash = Some(digest.into());
                    let path = s.project.output.display().to_string();
                    artifact_milestone |= s.progress_recovery.remember_artifact_path(path.clone());
                    if let Ok((sections, content_lines)) = tools::document_content_shape(&s.project)
                    {
                        if sections > s.progress_recovery.best_document_section_count {
                            s.progress_recovery.best_document_section_count = sections;
                            artifact_milestone = true;
                        }
                        if content_lines > s.progress_recovery.best_document_content_lines {
                            s.progress_recovery.best_document_content_lines = content_lines;
                            artifact_milestone = true;
                        }
                    }
                    if s.progress_recovery.remember_artifact(path, digest.into()) {
                        novel_artifact_change = true;
                        repetitions.clear();
                    }
                }
                if [
                    "file_read",
                    "file_list",
                    "source_search",
                    "symbol_search",
                    "document_inspect",
                ]
                .contains(&call.name.as_str())
                    && result["status"] == "ok"
                {
                    let args =
                        serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
                    let signature = format!("{}:{}:{}", call.name, args, result["data"]["hash"]);
                    let count = repetitions.entry(signature).or_default();
                    *count += 1;
                    if *count >= s.config.stall_round_limit {
                        repeated_read_detected = true;
                    }
                }

                if result["status"] != "ok"
                    && let Some(cp) = &mut s.checkpoint
                {
                    // Count a failed batch once, preserving its first cause rather
                    // than replacing it with a secondary acknowledgement failure.
                    if !checkpoint_failure_recorded {
                        checkpoint_failure_recorded = true;
                        cp.failed_attempts += 1;
                        cp.last_failure = Some(
                            result["error"]
                                .as_str()
                                .unwrap_or("tool operation failed")
                                .to_string(),
                        );
                    }
                    cp.failed = true;
                    cp.acknowledged = false;
                }
                // Share the remaining budget fairly; early large reads must not
                // starve later reads into archive-only responses.
                let slots = execution_calls.len() - messages.len() + 1;
                let budget = remaining
                    .checked_div(slots)
                    .unwrap_or(0)
                    .min(s.config.result_tokens)
                    .max(200);
                let result = tools::limit_result(&mut s, call, result, budget);
                tools::record_delivered_read(&mut s, call, &result);
                if result["status"] == "ok"
                    && [
                        "file_list",
                        "source_search",
                        "symbol_search",
                        "code_outline",
                        "document_inspect",
                        "db_query",
                    ]
                    .contains(&call.name.as_str())
                    && result["data"]["suppressed"] != true
                {
                    novel_navigation_evidence |= s
                        .progress_recovery
                        .remember_navigation(&call.name, &result["data"]);
                }
                if !checkpoint_batch {
                    tools::completion_review::observe(&mut s, call, &result);
                }
                if cache_parallel_result {
                    // Parallel reads run in temporary sessions, so their
                    // run_call ledger entries cannot be merged safely until
                    // source IDs and archive/cursor IDs have been remapped.
                    // Cache the final owner-session representation so a
                    // provider replaying the same call ID remains idempotent.
                    s.ledger.insert(
                        call.id.clone(),
                        (format!("{}:{}", call.name, call.arguments), result.clone()),
                    );
                }
                remaining =
                    remaining.saturating_sub(tools::result_tokens(call, &result, &s.config.model));
                emit(
                    &events,
                    AgentEvent::Tool {
                        session: s.id.clone(),
                        name: call.name.clone(),
                        status: result["status"].as_str().unwrap_or("error").into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                messages.push(
                    json!({"role":"tool","tool_call_id":call.id,"content":result.to_string()}),
                );
            }
            for call in &execution_calls[i..i + group] {
                seen_call_ids.insert(call.id.clone());
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
        }
        let new_source_evidence = s.sources.len() > prior_source_count;
        let written_progress = s
            .investigations
            .iter()
            .filter(|i| i.status == "written")
            .count()
            > prior_written_count;
        let pending_todos = s.task.todos.iter().filter(|item| !item.done).count();
        let plan_advanced = prior_current_todo.is_some()
            && pending_todos < prior_pending_todos
            && s.task.current_todo().map(|item| &item.id) != prior_current_todo.as_ref();
        let inventory_progress = tools::capabilities::progress(&s)
            .iter()
            .zip(prior_inventory_progress)
            .any(|(now, before)| *now > before);
        let verified_progress = new_verified > verified_count || inventory_progress;
        if verified_progress || written_progress || plan_advanced || artifact_milestone {
            s.progress_recovery.artifact_edits_without_milestone = 0;
        } else if novel_artifact_change && !checkpoint_batch {
            s.progress_recovery.artifact_edits_without_milestone = s
                .progress_recovery
                .artifact_edits_without_milestone
                .saturating_add(1);
        }
        if novel_artifact_change || verified_progress {
            s.progress_recovery.recovery_reason = None;
            rounds_without_progress = 0;
            repeated_read_detected = false;
            finalization_attempts = 0;
            s.progress_recovery.finalization_attempts = 0;
        } else if !checkpoint_batch {
            rounds_without_progress = rounds_without_progress.saturating_add(1);
            if rounds_without_progress == s.config.stall_round_limit {
                emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"Work is repeating without an output change or new verification; focusing the next request on the current outcome.".into() }, &cancel, run_deadline(started, &s.config)).await;
            }
        }
        if novel_artifact_change
            || verified_progress
            || new_source_evidence
            || novel_navigation_evidence
            || plan_advanced
        {
            s.progress_recovery.repeated_outcome_rounds = 0;
        } else if !checkpoint_batch {
            // A successful tool envelope can still contain applied=false or a
            // no-op plan edit. Count its actual effect, not the envelope status.
            s.progress_recovery.repeated_outcome_rounds = s
                .progress_recovery
                .repeated_outcome_rounds
                .saturating_add(1);
        }
        if novel_artifact_change || verified_progress || written_progress || new_source_evidence {
            s.progress_recovery.rounds_without_substantive_progress = 0;
        } else if !checkpoint_batch {
            s.progress_recovery.rounds_without_substantive_progress = s
                .progress_recovery
                .rounds_without_substantive_progress
                .saturating_add(1);
        }
        if verified_progress || artifact_milestone || written_progress || new_source_evidence {
            s.completion_review.repair_rounds = 0;
            s.completion_review.stalled_reviews = 0;
        }
        s.progress_recovery.rounds_without_progress = rounds_without_progress;
        s.progress_recovery.repeated_read = repeated_read_detected;
        s.run_guidance["progress_recovery"] = json!({
            "active":s.checkpoint.is_none() && (repeated_read_detected
                || rounds_without_progress >= s.config.stall_round_limit
                || s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit),
            "focused":s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit
                || s.progress_recovery.rounds_without_substantive_progress >= artifact_focus_limit(&s.config)
                || s.progress_recovery.artifact_edits_without_milestone >= artifact_focus_limit(&s.config),
            "rounds_without_progress":rounds_without_progress,
            "rounds_without_substantive_progress":s.progress_recovery.rounds_without_substantive_progress,
            "repeated_outcome_rounds":s.progress_recovery.repeated_outcome_rounds,
            "artifact_edits_without_milestone":s.progress_recovery.artifact_edits_without_milestone,
            "repeated_read":repeated_read_detected
        });
        verified_count = new_verified;
        if s.last_error
            .as_deref()
            .is_some_and(|error| error.starts_with("task_plan_pending:"))
            && s.task.current_todo().map(|item| item.id.as_str())
                != s.run_guidance["current_todo"]["id"].as_str()
        {
            s.last_error = None;
        }
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
        if s.history.bytes() > s.config.history_bytes.saturating_mul(2) {
            failure = Some("history_hard_limit: cleanup required".into());
            break;
        }
        // Bound ancillary session data too; never silently discard observations or receipts.
        if s.ancillary_bytes() > s.config.memory_bytes {
            failure = Some(
                "session_metadata_capacity: start another session or reduce retained details"
                    .into(),
            );
            break;
        }
        if !checkpoint_batch
            && s.completion_review
                .checks
                .iter()
                .any(|check| check.status != "met")
        {
            s.completion_review.repair_rounds = s.completion_review.repair_rounds.saturating_add(1);
            if s.completion_review.repair_rounds >= COMPLETION_REPAIR_EXHAUST_LIMIT
                && !recover_document(
                    &mut s,
                    "Repair rounds have not satisfied a completion check. Resolve the first check through a targeted edit or missing evidence, then request acceptance.",
                )
            {
                tools::completion_review::schedule_repairs(&mut s);
                s.status = "partial".into();
                s.last_error = Some("completion_review_no_progress: focused repair rounds did not satisfy another criterion; repair tasks and unmet checks retained for a changed approach".into());
                break;
            }
        }
        if !checkpoint_batch
            && repeated_outcome_exhausted(&s)
            && !recover_document(
                &mut s,
                "Repeated actions produced no new result. Use the existing evidence to repair or verify the current document section; change the failing arguments or action.",
            )
        {
            s.status = "partial".into();
            s.last_error = Some(REPEATED_OUTCOME_ERROR.into());
            break;
        }
        if !checkpoint_batch
            && s.progress_recovery.rounds_without_substantive_progress
                >= substantive_progress_limit(&s.config)
            && !recover_document(
                &mut s,
                "Navigation and bookkeeping are not completing the document. Write the current section from available evidence or read only its specific missing source range.",
            )
        {
            s.status = "partial".into();
            s.last_error = Some(NAVIGATION_STALL_ERROR.into());
            break;
        }
        if !checkpoint_batch
            && s.progress_recovery.artifact_edits_without_milestone
                >= artifact_exhaust_limit(&s.config)
            && !recover_document(
                &mut s,
                "Different rewrites have not completed a milestone. Verify the current section and resolve its concrete findings instead of another general rewrite.",
            )
        {
            s.status = "partial".into();
            s.last_error = Some(ARTIFACT_CHURN_ERROR.into());
            break;
        }
        if repair_edit_request
            && s.document_review.repair_requests >= s.config.document_repair_limit
        {
            if tools::document_review::rejected_on_current_result(&s) {
                // Failed/no-op edits did not create a new review target. Retain
                // the findings and renew the correction interval without a call.
                s.document_review.repair_requests = 0;
                recover_document(
                    &mut s,
                    "The edit interval left the reviewed document unchanged. Correct the failed/no-op edit using the tool result; the existing review findings still apply.",
                );
            } else {
                s.document_review.pending = true;
            }
        }
        snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
    }
    if cancel.is_cancelled() {
        s.status = "cancelled".into();
        s.last_error = None;
    } else if let Some(error) = failure {
        s.status = "blocked".into();
        s.last_error = Some(error);
    }
    s.activity = json!({"stage":"idle"});
    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
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
    if context::count(&request, &copy.config.model).saturating_add(copy.config.output_tokens)
        > copy.config.context_tokens
    {
        bail!("New context budget requires checkpoint first; current settings retained");
    }
    s.config = copy.config;
    Ok(())
}
