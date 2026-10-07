use crate::{
    config::Config,
    console,
    context::{self, ContextManager},
    llm::{LlmClient, OpenAiClient, ToolCall},
    session::Session,
    tools::{self, ToolRegistry},
};
use anyhow::{Result, bail};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod question;

const FINALIZATION_RETRY_LIMIT: usize = 8;
const COMPLETION_REVIEW_NO_PROGRESS_LIMIT: usize = 3;
const COMPLETION_REVIEW_EXHAUST_LIMIT: usize = 6;
const COMPLETION_REPAIR_ROUND_LIMIT: usize = 16;
const COMPLETION_REPAIR_EXHAUST_LIMIT: usize = 32;
const LENGTH_RECOVERY_LIMIT: usize = 8;
const REVIEW_RESPONSE_LIMIT: usize = 8;
/// Consecutive invalid verdicts after which a document-work review is skipped
/// for the current result and reported as unreviewed.
const REVIEW_UNAVAILABLE_LIMIT: usize = 3;
/// Model requests available once closing mode starts. The runtime finishes
/// the document itself when they are spent.
pub const CLOSING_ROUND_LIMIT: usize = 12;
const MAX_REPORTED_GAPS: usize = 30;

/// Requests without a new best progress score before closing mode. Earlier
/// stages (1x focus, 2x narrowed tools) use the same counter.
fn closing_stall_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(3)
}

/// One monotonic measure of document progress: the best document shape,
/// reduced review findings, met acceptance checks, and distinct source
/// evidence gathered while investigating, while cited ranges remain unread
/// or while a review repair is open. Plan bookkeeping is excluded;
/// completing and reopening the same to-do is not progress.
fn progress_score(s: &Session) -> usize {
    s.progress_recovery.best_document_section_count * 2
        + s.progress_recovery.best_document_content_lines
        + tools::document_review::progress(s)
        + s.completion_review.best_met * 2
        + s.progress_recovery.evidence_credit
}

/// Cited ranges of the saved document never delivered as complete lines of
/// the current file version; zero before the document exists.
fn unread_citation_count(s: &Session) -> usize {
    if !s.is_document_work() || !s.document_written {
        return 0;
    }
    tools::unread_citations(s).map_or(0, |unread| unread.len())
}

fn closing_instruction(s: &Session) -> String {
    let closing = s.progress_recovery.closing.as_ref();
    let remaining = CLOSING_ROUND_LIMIT.saturating_sub(closing.map_or(0, |c| c.rounds));
    if !s.document_written {
        return format!(
            "Closing mode: no document is saved yet and source reading is withheld. Create the requested document NOW with document_edit action=create (or document_edit_batch) from the evidence already gathered. Write every requested section; where a fact was not confirmed from delivered sources, say so in the text instead of guessing. Then give a concise final answer. At most {remaining} requests remain; without a saved document the run stops unfinished."
        );
    }
    format!(
        "Closing mode: finish the requested document now from the evidence already gathered; discovery tools are withheld. 1) Write any missing requested section from gathered evidence, stating in the text when a fact is unconfirmed. 2) For a cited range reported as unread (citation_check or document_audit unread_citation), file_read that range or narrow the citation to the lines already read; qualify a claim that cannot be supported in its section instead of describing it as verified. 3) Fix review findings confirmed for the current document or completion checks with targeted edits when possible; otherwise leave them, the runtime reports them as unresolved. 4) Complete or remove remaining to-dos with actual results, then give a concise final answer. At most {remaining} requests remain; afterwards the runtime finishes the document and lists every unresolved item. Do not invent evidence."
    )
}

/// Everything a complete_with_gaps result must disclose, derived from state.
fn collect_gaps(s: &mut Session, extra: &[String]) -> Vec<String> {
    let mut gaps: Vec<String> = extra.to_vec();
    if let Err(error) = tools::verify_document_write(s) {
        gaps.push(format!("문서 저장 확인 — {error}"));
    }
    for item in s.task.todos.iter().filter(|item| !item.done).take(10) {
        gaps.push(format!("미완료 할 일 — {}", item.text));
    }
    if s.is_document_work() && s.document_written {
        let unread = tools::unread_citations(s).unwrap_or_default();
        for range in unread.iter().take(12) {
            gaps.push(format!(
                "근거 미확인 — {}:{}-{} 범위를 읽지 않고 인용했습니다.",
                range["path"].as_str().unwrap_or_default(),
                range["start_line"],
                range["end_line"]
            ));
        }
        if let Ok(audit) = tools::audit_document(s)
            && audit["structural_ok"] != true
            && let Some(other) = audit["issue_count"]
                .as_u64()
                .map(|count| (count as usize).saturating_sub(unread.len()))
            && other > 0
        {
            gaps.push(format!(
                "근거 점검 — 구조 문제 {other}건이 남아 있습니다 (document_audit)."
            ));
        }
        if s.document_written && s.config.source_document_review && tools::citation_count(s) > 0 {
            let ranges = tools::document_review::unavailable_ranges(s);
            match tools::document_review::current_verdict(s) {
                tools::document_review::CurrentVerdict::Approved => {}
                tools::document_review::CurrentVerdict::Rejected(issues) => {
                    for issue in issues.iter().take(12) {
                        gaps.push(format!("문서 검토 지적 — {issue}"));
                    }
                }
                tools::document_review::CurrentVerdict::Unavailable => {
                    if ranges.is_empty() {
                        gaps.push("문서 검토 — 검토 응답 오류로 검토를 마치지 못했습니다.".into());
                    }
                }
                tools::document_review::CurrentVerdict::Unreviewed => {
                    gaps.push("문서 검토 — 현재 문서를 마감 전에 검토하지 못했습니다.".into());
                }
            }
            if !ranges.is_empty() {
                let ranges = ranges
                    .iter()
                    .map(|(start, end)| format!("{start}–{end}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                gaps.push(format!(
                    "문서 검토 — {ranges}줄은 검토를 마치지 못했습니다."
                ));
            }
        }
    }
    if tools::completion_review::required(s) {
        use tools::completion_review::{self as review, Check, CurrentVerdict};
        let status = |check: &Check| {
            if check.status == "unmet" {
                "미충족"
            } else {
                "확인 불가"
            }
        };
        match review::current_verdict(s) {
            CurrentVerdict::Approved => {}
            CurrentVerdict::Rejected(checks) => {
                for check in checks.iter().filter(|check| check.status != "met").take(12) {
                    gaps.push(format!(
                        "완료 조건 {} ({}) — {}",
                        check.id,
                        status(check),
                        check.reason
                    ));
                }
            }
            CurrentVerdict::Unavailable => {
                gaps.push("완료 조건 검증 — 검토 응답 오류로 검증을 마치지 못했습니다.".into());
            }
            // Only evidence or task records changed after the last review, so
            // its unmet checks still describe the saved result. A live run
            // reported five rejections as "not reviewed" after a task_state
            // edit. Changed files may have repaired them: report no reason.
            CurrentVerdict::Unreviewed => match review::prior_rejection(s) {
                Some(prior) if review::reviewed_files_unchanged(s) => {
                    gaps.push("완료 조건 검증 — 마지막 검토 뒤 산출물은 그대로이고 근거·작업 기록만 바뀌어 다시 검토하지 못했습니다.".into());
                    for check in prior.checks.iter().take(12) {
                        gaps.push(format!(
                            "완료 조건 {} (마지막 검토: {}) — {}",
                            check.id,
                            status(check),
                            check.reason
                        ));
                    }
                }
                _ => {
                    gaps.push("완료 조건 검증 — 현재 결과를 마감 전에 검토하지 못했습니다.".into());
                }
            },
        }
    }
    for item in &s.task.unresolved {
        gaps.push(format!("미확인 사항 — {item}"));
    }
    let mut seen = std::collections::BTreeSet::new();
    gaps.retain(|gap| seen.insert(gap.clone()));
    if gaps.len() > MAX_REPORTED_GAPS {
        let omitted = gaps.len() - MAX_REPORTED_GAPS;
        gaps.truncate(MAX_REPORTED_GAPS);
        gaps.push(format!("… 외 {omitted}건 (진행 패널 참고)"));
    }
    gaps
}

// A document whose citations are all read can still lack requested sections;
// a live run read an older wording as permission to remove the to-dos for
// unwritten sections.
const PLAN_CLOSEOUT_INSTRUCTION: &str = "The document is written and its cited ranges are read, but the to-dos in plan_closeout are open and the final answer is refused while any is open. A to-do whose work is not done yet, such as a requested section still missing from the document, is NOT obsolete. Do that work first: read the evidence and write the section. When the listed work is done, close them in ONE task_plan apply using plan_closeout.expected_revision: one operation per item in the listed order, complete with the actual observed result, or remove with a reason only when the item is genuinely obsolete, never merely unstarted. complete must follow list order, because only the current item can complete. Then give the final answer.";

const REVIEW_REPAIR_RESUME_INSTRUCTION: &str = "A checkpoint cleared the context during review repair, and the document is still UNCHANGED: every finding in review_repair.unrepaired_findings is still open. A final answer now is rejected again without a new review. Edit the document for these findings first.";

const READY_FOR_FINAL_INSTRUCTION: &str = "Ready to finish, provided every requested section is written: the document is saved, every cited range was read, no to-do remains and no review finding is open for this document version. If a requested section is still missing, write it first. Give the concise final answer now (output path, verification scope, remaining limitations). The runtime then runs the document review and completion checks and returns any finding as a repair. Do not inspect or audit again unless you change the document.";

const REVIEW_REPAIR_INSTRUCTION: &str = "Review repair: fix ALL findings in document_review.issues before the next final answer. Read any source range a finding needs, then apply the corrections with as few document edits as possible: group non-overlapping corrections in one document_edit_batch whose single expected_hash is the top-level argument (never inside edits). Operations apply in order, so never target text that an earlier operation in the same batch replaces; when corrections touch the same passage, merge them into one operation or use a separate request. Then give the final answer to start the re-review. Do not alternate single edits with document_audit or document_inspect. Fix findings in their original sections; do not add a review-notes section.";

/// A completed document review rejected the result and no re-review is
/// running: the next work is repairing its findings.
fn review_repair_pending(s: &Session) -> bool {
    s.checkpoint.is_none()
        && s.is_document_work()
        && s.document_written
        && s.config.source_document_review
        && matches!(
            tools::document_review::current_verdict(s),
            tools::document_review::CurrentVerdict::Rejected(_)
        )
}

/// During review repair an audit between edits needs only its verdict and a
/// few issues; the full list is available by paging with offset.
fn compact_repair_audit(s: &Session, call: &ToolCall, mut result: Value) -> Value {
    const REPAIR_AUDIT_ISSUES: usize = 5;
    if call.name != "document_audit" || result["status"] != "ok" || !review_repair_pending(s) {
        return result;
    }
    if let Some(issues) = result["data"]["issues"].as_array_mut()
        && issues.len() > REPAIR_AUDIT_ISSUES
    {
        let offset = serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|args| args["offset"].as_u64())
            .unwrap_or(0) as usize;
        issues.truncate(REPAIR_AUDIT_ISSUES);
        result["data"]["next_offset"] = json!(offset + REPAIR_AUDIT_ISSUES);
        result["data"]["compacted_for_review_repair"] = json!(true);
    }
    result
}

/// Document work whose own bookkeeping is finished: the next useful step is
/// the final answer, which starts the runtime's review and acceptance checks.
fn ready_for_final(s: &mut Session) -> bool {
    s.task.current_todo().is_none() && ready_except_plan(s)
}

/// A run ready for its final answer has changed the rejected result and
/// closed its repair to-dos, and that answer starts the next review; closing
/// mode asks for the answer too. The repair message telling the model not
/// to answer again would contradict either instruction, so this request
/// drops it; last_review keeps the checks themselves until that review.
fn settle_completion_error(s: &mut Session) {
    if s.run_guidance["completion_error"]
        .as_str()
        .is_some_and(|error| error.starts_with("completion_review_unmet:"))
    {
        s.run_guidance["completion_error"] = Value::Null;
    }
}

/// Everything the final answer needs except that to-dos remain open. The
/// final answer is refused until they close, and closing them one per request
/// only spends rounds, so the runtime lists them for one batched update.
fn ready_except_plan(s: &mut Session) -> bool {
    if let Some(guidance) = s.run_guidance.as_object_mut() {
        guidance.remove("document_readiness");
    }
    let bookkeeping_ready = s.checkpoint.is_none()
        && s.is_document_work()
        && s.document_written
        && !s.document_review.pending
        && !s.completion_review.pending
        && !tools::document_review::rejected_on_current_result(s)
        && !tools::completion_review::rejected_on_current_result(s)
        && tools::verify_document_write(s).is_ok();
    if !bookkeeping_ready {
        return false;
    }
    // Every citation must be well-formed and read. Use the same audit as
    // final acceptance, including its freshness revalidation.
    match tools::audit_document(s) {
        Ok(audit) if audit["structural_ok"] == true => true,
        Ok(mut audit) => {
            if let Some(issues) = audit["issues"].as_array_mut() {
                issues.truncate(5);
            }
            audit["next_offset"] =
                json!((audit["issue_count"].as_u64().unwrap_or(0) > 5).then_some(5));
            s.run_guidance["document_readiness"] = audit;
            false
        }
        Err(error) => {
            s.run_guidance["document_readiness"] =
                json!({"structural_ok":false,"error":error.to_string()});
            false
        }
    }
}

/// The open to-dos in order, bounded by one task_plan batch.
fn plan_closeout(s: &Session) -> Value {
    let pending: Vec<_> = s.task.todos.iter().filter(|item| !item.done).collect();
    json!({
        "expected_revision":s.task.plan_revision,
        "pending_count":pending.len(),
        "items":pending.iter().take(16).map(|item| json!({"id":item.id,"text":item.text})).collect::<Vec<_>>(),
        "batch_limit":16
    })
}

/// An outline or audit identical to one already in active context carries no
/// new information. Return a short marker instead of the same payload, so a
/// repeated check costs little context and does not look like progress.
fn suppress_unchanged_repeat(
    s: &Session,
    pending: &[Value],
    call: &ToolCall,
    result: Value,
) -> Value {
    if !matches!(call.name.as_str(), "document_inspect" | "document_audit")
        || result["status"] != "ok"
        || !result["data"].is_object()
    {
        return result;
    }
    let seen = s
        .history
        .bundles
        .iter()
        .filter(|bundle| bundle.active)
        .flat_map(|bundle| &bundle.messages)
        .chain(pending)
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str::<Value>(content).ok())
        .any(|prior| prior["status"] == "ok" && prior["data"] == result["data"]);
    if !seen {
        return result;
    }
    json!({"status":"ok","data":{"unchanged":true,"suppressed":true,
        "hash":result["data"]["hash"],
        "guidance":"Identical result is already in active context: the document and its evidence have not changed since. Use that result and act on it: edit, read an unread cited range, or give the final answer. Do not repeat this check until something changes."}})
}

/// Abandon a pending review whose responses keep failing validation, for
/// document work only. In closing mode the first failure is enough. The
/// result then finishes without that review and reports it as unchecked.
/// Outside closing mode a document review skips only the failing page and
/// keeps the findings of other pages. Returns the notice to show, if any.
fn abandon_failing_review(s: &mut Session, failures: usize) -> Option<&'static str> {
    if !s.is_document_work()
        || s.checkpoint.is_some()
        || (failures < REVIEW_UNAVAILABLE_LIMIT && s.progress_recovery.closing.is_none())
    {
        return None;
    }
    if s.document_review.pending
        && !s.completion_review.pending
        && s.progress_recovery.closing.is_none()
    {
        use tools::document_review::PageSkip;
        match tools::document_review::skip_failing_page(s) {
            PageSkip::NotApplicable => {}
            PageSkip::Continued => {
                s.last_error = None;
                s.progress_recovery.action_required = false;
                note_document_review_verdict(s);
                return Some(REVIEW_PAGE_SKIPPED_NOTICE);
            }
            PageSkip::Unavailable => {
                s.last_error = Some(DOCUMENT_REVIEW_UNAVAILABLE.into());
                s.progress_recovery.action_required = false;
                return Some(REVIEW_UNAVAILABLE_NOTICE);
            }
        }
    }
    if s.completion_review.pending {
        // last_error holds the rejected response's validation error; keep it
        // before the notice below replaces it.
        let reason = s.last_error.clone();
        tools::completion_review::mark_unavailable(s, reason.clone());
        s.last_error = Some(format!(
            "completion_review_unavailable: acceptance review responses were invalid ({}); give the final answer again and the result will be reported as unchecked",
            reason.as_deref().unwrap_or("no validation error recorded")
        ));
    } else if s.document_review.pending {
        tools::document_review::mark_unavailable(s);
        s.last_error = Some(DOCUMENT_REVIEW_UNAVAILABLE.into());
    } else {
        return None;
    }
    s.progress_recovery.action_required = false;
    // Closing allows a single review response; "repeatedly" would misstate it.
    Some(if failures < REVIEW_UNAVAILABLE_LIMIT {
        REVIEW_CLOSING_UNAVAILABLE_NOTICE
    } else {
        REVIEW_UNAVAILABLE_NOTICE
    })
}

/// After a document review verdict with findings, require their repair.
fn note_document_review_verdict(s: &mut Session) {
    if s.document_review.pending || s.document_review.issues.is_empty() {
        return;
    }
    s.last_error = Some(format!(
        "document_review: {}",
        s.document_review.issues.join("; ")
    ));
    s.progress_recovery.action_required = true;
    if s.document_review.stalled_attempts >= s.config.review_limit {
        recover_document(
            s,
            "Document review still has unresolved findings. Correct the first finding in its original section; do not request another unchanged review.",
        );
    }
}

/// Consecutive empty replies retried before a non-document run stops.
const EMPTY_COMPLETION_RETRIES: usize = 2;

/// Consecutive empty replies after which document work enters closing.
/// Document recovery used to retry an identical request until the stall
/// ladder closed it: a live run spent 24 rounds (1.4M input tokens) there.
const EMPTY_DOCUMENT_CLOSING: usize = 3;

/// Unchanged-document final answers rejected by a review before closing.
/// A live run closed after two with most of its budget left.
const UNREPAIRED_FINAL_LIMIT: usize = 3;

/// The output document's current hash, if it exists.
fn output_hash(s: &Session) -> Option<String> {
    tools::output_path(&s.project)
        .and_then(|path| tools::hash_file(&path))
        .ok()
}

/// Count a final answer rejected because the reviewed document was not
/// edited. An edit changes the document hash and restarts the count.
fn note_unrepaired_final(s: &mut Session) -> usize {
    let current = output_hash(s);
    let recovery = &mut s.progress_recovery;
    if current.is_some() && recovery.unrepaired_final_hash == current {
        recovery.unrepaired_finals = recovery.unrepaired_finals.saturating_add(1);
    } else {
        recovery.unrepaired_final_hash = current;
        recovery.unrepaired_finals = 1;
    }
    recovery.unrepaired_finals
}

const REVIEW_UNAVAILABLE_NOTICE: &str = "검토 응답이 반복해서 형식에 맞지 않아 이 결과의 검토를 생략하고, 완료 보고에 미검토로 표시합니다.";
const REVIEW_CLOSING_UNAVAILABLE_NOTICE: &str = "마감 단계의 검토 응답이 형식에 맞지 않아 이 결과의 검토를 생략하고, 완료 보고에 미검토로 표시합니다.";
const DOCUMENT_REVIEW_UNAVAILABLE: &str = "document_review_unavailable: document review responses were invalid; give the final answer again and the document will be reported as unreviewed";
const REVIEW_PAGE_SKIPPED_NOTICE: &str =
    "검토 응답이 반복해서 형식에 맞지 않아 이 부분의 검토를 건너뛰고, 나머지 검토를 이어갑니다.";

fn finish_cause_text(cause: &str) -> &'static str {
    match cause {
        "run_budget_exhausted" => "실행 예산이 소진되어 마감했습니다.",
        "run_timeout" => "실행 시간이 끝나 마감했습니다.",
        "closing_round_limit" => "마감 단계의 요청 한도에 도달해 마감했습니다.",
        "budget" => "마감 예산에 도달해 마감했습니다.",
        "stall" => "진행이 오래 멈춰 마감했습니다.",
        "review_unrepaired" => "검토 지적이 반영되지 않은 채 최종 답변이 반복돼 마감했습니다.",
        "empty_response" => "모델이 빈 응답을 반복해 마감했습니다.",
        _ => "마감했습니다.",
    }
}

fn gap_report(s: &Session, answer: Option<&str>, cause: Option<&str>) -> String {
    let mut text = match answer.map(str::trim).filter(|answer| !answer.is_empty()) {
        Some(answer) => answer.to_owned(),
        None => format!(
            "`{}` 문서 작성을 마쳤습니다.",
            crate::paths::display_path(&s.project.output)
        ),
    };
    if s.completion_gaps.is_empty() {
        return text;
    }
    let cause = cause.map_or("", finish_cause_text);
    text.push_str(&format!(
        "\n\n---\n**확인하지 못한 항목 {}건** {cause} 결과는 완료로 처리했지만 아래 항목은 검증되지 않았습니다.\n",
        s.completion_gaps.len()
    ));
    for gap in &s.completion_gaps {
        text.push_str(&format!("- {}\n", crate::paths::display_text(gap)));
    }
    text
}

/// A result can finish with reported gaps only when this task saved the
/// document, it has body text beyond headings, and it still matches the
/// agent's last write. A pre-existing, missing, empty or externally changed
/// file keeps the run from finishing.
fn document_saved(s: &Session) -> bool {
    s.document_written
        && tools::document_content_shape(&s.project)
            .is_ok_and(|(headings, content_lines)| content_lines > headings)
        && tools::verify_document_write(s).is_ok()
}

/// Finish document work without a model call, reporting what remains
/// unresolved. Returns the final message, or None when there is no saved
/// document to finish (the caller then keeps its original stop reason).
fn force_finish(s: &mut Session, cause: &str) -> Option<String> {
    if !s.is_document_work() || s.checkpoint.is_some() || !document_saved(s) {
        return None;
    }
    s.set_run_stop_reason(cause);
    if s.document_review.pending {
        tools::document_review::defer_for_repair(s);
    }
    s.completion_review.pending = false;
    s.continuation = None;
    s.completion_gaps = collect_gaps(s, &[]);
    s.status = if s.completion_gaps.is_empty() {
        "complete"
    } else {
        "complete_with_gaps"
    }
    .into();
    s.last_error = None;
    Some(gap_report(s, None, Some(cause)))
}

async fn publish_final(
    s: &mut Session,
    text: String,
    events: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
    started: Instant,
) {
    s.history.push(vec![assistant(&text, &[])], true);
    emit(
        events,
        AgentEvent::Delta {
            session: s.id.clone(),
            text,
        },
        cancel,
        run_deadline(started, &s.config),
    )
    .await;
}

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

// Resuming keeps the no-progress counters (see ProgressRecovery), so the
// message must not suggest that an unchanged "continue" can proceed.
macro_rules! exhausted_next_step {
    () => {
        "; resuming continues the same no-progress count and stops again unless its first step yields a new result, so start a new task that changes the approach or narrows the scope"
    };
}
const REPEATED_OUTCOME_ERROR: &str = concat!(
    "progress_recovery_exhausted: repeated responses or tool results produced no new output, source evidence or verified result; current work is retained",
    exhausted_next_step!()
);
const NAVIGATION_STALL_ERROR: &str = concat!(
    "progress_recovery_exhausted: navigation and bookkeeping did not produce source evidence, a changed artifact or a verified result; current work is retained",
    exhausted_next_step!()
);
const ARTIFACT_CHURN_ERROR: &str = concat!(
    "artifact_progress_exhausted: many distinct edits produced no completed task item, verified section or improved completion check; current files and requirements are retained",
    exhausted_next_step!()
);

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
    if !matches!(
        code,
        "tool_call_batch_limit"
            | "invalid_tool_arguments"
            | "malformed_tool_call"
            | "response_size_limit"
    ) {
        return false;
    }
    // Nothing in a rejected response ran, so outside document work it gets
    // one guided retry too. Only once in a row: the reason clears after the
    // next executed batch (a live run ended here after 32 rounds), so a
    // second consecutive rejection still stops the run.
    if !s.is_document_work() && s.progress_recovery.recovery_reason.is_some() {
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
    let guidance = if code == "response_size_limit" {
        format!(
            "{reason}; none of this response executed. Keep the next response concise and split large document edits into smaller complete calls within the output limit."
        )
    } else {
        format!(
            "{reason}; none of this batch executed. Reissue necessary calls in batches of at most {limit} with valid JSON object arguments, short unique call IDs and exact available tool names."
        )
    };
    s.last_error = Some(reason.into());
    // An oversized text-only final can be fixed by shortening the answer;
    // requiring a tool call here would delay acceptance for a finished file.
    s.progress_recovery.action_required |= code != "response_size_limit";
    // Nothing ran, so the retry is safe; run_guidance carries the reason.
    s.progress_recovery.recovery_reason = Some(guidance);
    true
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
/// Waits before resending a request whose provider stayed unavailable through
/// the client's quick retries; the run deadline still bounds them.
const PROVIDER_OUTAGE_WAITS: [u64; 4] = [10, 30, 60, 120];

fn run_deadline(started: Instant, config: &Config) -> tokio::time::Instant {
    tokio::time::Instant::from_std(started)
        .checked_add(Duration::from_secs(config.run_timeout_secs))
        .unwrap_or_else(tokio::time::Instant::now)
}

fn remap_read_ids(
    value: &mut Value,
    sources: &std::collections::BTreeMap<String, String>,
    cursors: &std::collections::BTreeMap<String, String>,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                remap_read_ids(value, sources, cursors);
            }
        }
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if matches!(key.as_str(), "source" | "signature_source")
                    && let Some(source) = value.as_object_mut()
                    && let Some(id) = source.get("id").and_then(Value::as_str).map(str::to_owned)
                    && let Some(canonical) = sources.get(&id)
                {
                    source.insert("id".into(), json!(canonical));
                }
                if key == "cursor"
                    && let Some(canonical) = value.as_str().and_then(|id| cursors.get(id))
                {
                    *value = json!(canonical);
                }
                remap_read_ids(value, sources, cursors);
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
    // Reserve before cloning: a slow or disconnected consumer must not hold
    // an extra full session while waiting for queue space.
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => tx.try_reserve().ok(),
        _ = tokio::time::sleep_until(deadline) => tx.try_reserve().ok(),
        permit = tx.reserve() => permit.ok(),
    };
    if let Some(permit) = permit {
        permit.send(AgentEvent::Snapshot(Box::new(s.clone())));
    }
}
fn assistant(text: &str, calls: &[ToolCall]) -> Value {
    let mut v = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        v["tool_calls"]=json!(calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}})).collect::<Vec<_>>());
    }
    v
}
#[derive(Clone, Copy)]
struct ToolDeadline {
    at: tokio::time::Instant,
    reason: &'static str,
}

impl ToolDeadline {
    fn new(timeout: Duration, run_deadline: tokio::time::Instant) -> Self {
        let tool_deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or(run_deadline);
        if run_deadline <= tool_deadline {
            Self {
                at: run_deadline,
                reason: "run_timeout",
            }
        } else {
            Self {
                at: tool_deadline,
                reason: "tool_timeout",
            }
        }
    }
}

async fn execute_one(
    s: Session,
    call: ToolCall,
    cancel: &CancellationToken,
    run_deadline: tokio::time::Instant,
) -> (Session, Value) {
    if s.write_outcome_uncertain.load(Ordering::Acquire) {
        return (
            s,
            tools::envelope(Err(anyhow::anyhow!(
                "tool_worker_unresolved: previous external write outcome is unknown; inspect changes and restart before another run"
            ))),
        );
    }
    if cancel.is_cancelled() {
        return (s, tools::envelope(Err(anyhow::anyhow!("cancelled"))));
    }
    if tokio::time::Instant::now() >= run_deadline {
        return (s, tools::envelope(Err(anyhow::anyhow!("run_timeout"))));
    }
    let deadline = ToolDeadline::new(
        Duration::from_secs(s.config.tool_timeout_secs),
        run_deadline,
    );
    let backup = s.clone();
    let child = cancel.child_token();
    let external_write = tools::external_write(&call.name);
    let receiver = match spawn_tool_worker(s, call, child.clone()) {
        Ok(receiver) => receiver,
        Err(error) => return (backup, tools::envelope(Err(error.into()))),
    };
    await_tool_worker(
        backup,
        receiver,
        cancel,
        child,
        deadline,
        Duration::from_secs(1),
        external_write,
    )
    .await
}

type ToolOutcome = (Session, Value);

// Normal web admission allows 32 sessions with up to four parallel reads.
// Timed-out OS calls can outlive their receiver, so count actual threads, not
// awaiting runs. A permit is held until the thread and its Session finish.
const MAX_TOOL_WORKERS: usize = 128;

fn tool_workers() -> Arc<tokio::sync::Semaphore> {
    static WORKERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    WORKERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_TOOL_WORKERS)))
        .clone()
}

fn spawn_tool_thread<T: Send + 'static>(
    workers: Arc<tokio::sync::Semaphore>,
    operation: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<oneshot::Receiver<T>> {
    crate::worker::spawn(
        workers,
        "mnemoarc-tool",
        "tool_worker_capacity: previous tool threads are still running; wait for them to finish before retrying",
        operation,
    )
}

fn spawn_tool_worker(
    mut session: Session,
    call: ToolCall,
    cancel: CancellationToken,
) -> std::io::Result<oneshot::Receiver<ToolOutcome>> {
    spawn_tool_thread(tool_workers(), move || {
        let result = tools::run_call_cancellable(&mut session, &call, &cancel);
        (session, result)
    })
}

async fn await_tool_worker(
    backup: Session,
    mut receiver: oneshot::Receiver<ToolOutcome>,
    cancel: &CancellationToken,
    child: CancellationToken,
    deadline: ToolDeadline,
    settle_wait: Duration,
    external_write: bool,
) -> ToolOutcome {
    let _worker_cancel = child.clone().drop_guard();
    let stopped = tokio::select! {
        biased;
        outcome = &mut receiver => return received_tool_outcome(backup, outcome, external_write),
        _ = cancel.cancelled() => "cancelled",
        _ = tokio::time::sleep_until(deadline.at) => deadline.reason,
    };
    child.cancel();
    // A write can finish just after cancellation. Give it a bounded chance
    // to report the actual result before declaring its outcome unknown.
    if external_write {
        if let Ok(outcome) = tokio::time::timeout(settle_wait, &mut receiver).await {
            let (session, mut result) = received_tool_outcome(backup, outcome, true);
            // A queued writer sees a cancelled child token for both user
            // cancellation and deadlines. Preserve the agent's stop reason
            // when the worker confirms it stopped (or rolled back). Otherwise
            // a timeout is reported as user cancellation with recovery=stop,
            // and bypasses timeout/failure handling. Keep actual commits and
            // uncertain outcomes exactly as reported by the worker.
            if stopped != "cancelled" && result["status"] == "cancelled" {
                let message = format!(
                    "{stopped}: {}",
                    result["error"].as_str().unwrap_or("cancelled")
                );
                result["status"] = json!("error");
                result["error"] = json!(message);
                result["recovery"] = tools::recovery::describe(&message);
            }
            return (session, result);
        }
        backup
            .write_outcome_uncertain
            .store(true, Ordering::Release);
        return (
            backup,
            tools::envelope(Err(anyhow::anyhow!(
                "tool_worker_unresolved: {stopped}; external write outcome is unknown; review files or database changes and restart before another run"
            ))),
        );
    }
    (backup, tools::envelope(Err(anyhow::anyhow!(stopped))))
}

fn received_tool_outcome(
    backup: Session,
    outcome: Result<ToolOutcome, oneshot::error::RecvError>,
    external_write: bool,
) -> ToolOutcome {
    match outcome {
        Ok((session, result)) => {
            // A worker can return normally while reporting an uncertain commit,
            // failed rollback or a caught panic. Returning an error does not
            // establish that its external writes were undone.
            if external_write
                && result["error"]
                    .as_str()
                    .is_some_and(tools::uncertain_write_error)
            {
                session
                    .write_outcome_uncertain
                    .store(true, Ordering::Release);
            }
            (session, result)
        }
        Err(_) => {
            if external_write {
                backup
                    .write_outcome_uncertain
                    .store(true, Ordering::Release);
            }
            (
                backup,
                tools::envelope(Err(anyhow::anyhow!(
                    "tool_worker_panic: worker lost; last snapshot retained, write outcomes require review"
                ))),
            )
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
    // An explicit hash is the model's optimistic-lock precondition. Even if
    // an earlier call in this response changed the document, never replace
    // that precondition: the second edit may have been planned for the old
    // version and must get a revision conflict instead of silently applying.
    if object.contains_key("expected_hash") {
        return (call.clone(), false);
    }
    // A create call intentionally has no revision precondition: rebasing it
    // would turn its useful document_exists error into a less meaningful
    // stale-hash error. A first write on a missing file, however, may be
    // followed by another write in the same response, so fill its now
    // required precondition just like append/patch/section calls.
    if action == "create" {
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
    run_deadline: tokio::time::Instant,
) -> Vec<Value> {
    use futures_util::{StreamExt, stream};
    let project = s.project.clone();
    let config = s.config.clone();
    let active = s.active_tools.clone();
    let history = if calls.iter().any(|call| call.name == "file_read") {
        tools::parallel_read_history(&s.history)
    } else {
        crate::session::SessionHistory {
            next_id: s.history.next_id,
            pruned_through: s.history.pruned_through,
            ..Default::default()
        }
    };
    let guidance = s.run_guidance.clone();
    let file_cursors = s.file_cursors.clone();
    let list_cursor_scopes = s.list_cursor_scopes.clone();
    let owned_calls = calls.to_vec();
    let futures = owned_calls.into_iter().map(|call| {
        let mut temporary = Session::new(project.clone(), config.clone());
        temporary.active_tools = active.clone();
        temporary.history = history.clone();
        temporary.run_guidance = guidance.clone();
        temporary.file_cursors = file_cursors.clone();
        temporary.list_cursor_scopes = list_cursor_scopes.clone();
        let cancel = cancel.clone();
        let timeout = config.tool_timeout_secs;
        async move {
            // Buffered calls may start well after their batch was admitted.
            // Check the shared deadline before creating each worker.
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            if tokio::time::Instant::now() >= run_deadline {
                bail!("run_timeout");
            }
            let deadline = ToolDeadline::new(Duration::from_secs(timeout), run_deadline);
            let child = cancel.child_token();
            let _worker_cancel = child.clone().drop_guard();
            let mut receiver = spawn_tool_worker(temporary, call, child.clone())
                .map_err(|error| {
                    if error.to_string().starts_with("tool_worker_capacity:") {
                        anyhow::Error::from(error)
                    } else {
                        anyhow::anyhow!(
                            "tool_worker_start_failed: {error}; the tool did not start and nothing was executed"
                        )
                    }
                })?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    child.cancel();
                    Err(anyhow::anyhow!("cancelled"))
                }
                result = &mut receiver => {
                    result.map_err(|error| anyhow::anyhow!("tool_worker_panic: tool worker failed: {error}"))
                }
                _ = tokio::time::sleep_until(deadline.at) => {
                    child.cancel();
                    Err(anyhow::anyhow!(deadline.reason))
                }
            }
        }
    });
    let mut results = vec![];
    let mut pending = stream::iter(futures).buffered(config.read_parallelism);
    while let Some(result) = pending.next().await {
        match result {
            Ok((temp, mut result)) => {
                let mut cursor_ids = std::collections::BTreeMap::new();
                for (id, cursor) in temp.file_cursors {
                    // Workers start with the owner's immutable cursors. Only
                    // newly issued positions need interning during the merge.
                    if s.file_cursors.contains_key(&id) {
                        continue;
                    }
                    if let Some((canonical, _)) = s
                        .file_cursors
                        .iter()
                        .find(|(_, existing)| **existing == cursor)
                    {
                        cursor_ids.insert(id, canonical.clone());
                    } else {
                        s.file_cursors.insert(id, cursor);
                    }
                }
                for (fingerprint, scope) in temp.list_cursor_scopes {
                    if !list_cursor_scopes
                        .iter()
                        .any(|(known, _)| *known == fingerprint)
                    {
                        s.remember_list_scope(fingerprint, scope);
                    }
                }
                let mut source_ids = std::collections::BTreeMap::new();
                for (_, source) in temp.sources {
                    let source_id = source.id.clone();
                    let canonical = s
                        .sources
                        .values()
                        .find(|existing| tools::same_source(existing, &source))
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
                remap_read_ids(&mut result, &source_ids, &cursor_ids);
                // Temporary history IDs cannot escape into the owning session.
                if result["truncated"] == true
                    && let Some(id) = result["archive_id"]
                        .as_u64()
                        .or_else(|| result["next_cursor"]["id"].as_u64())
                    && let Ok(bundle) = temp.history.read(id)
                {
                    // The archive was created in a temporary read session.
                    // Its result can contain source objects whose IDs were
                    // allocated there, so remap sources and cursor aliases in
                    // archived messages too. History continuations must use
                    // the same canonical IDs as the delivered result.
                    let mut messages = bundle.messages.clone();
                    for message in &mut messages {
                        remap_read_ids(message, &source_ids, &cursor_ids);
                    }
                    let id = s.history.push_shared(messages, true);
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
    let started = Instant::now();
    let initial_tokens = s.input_tokens.saturating_add(s.output_tokens);
    if s.write_outcome_uncertain.load(Ordering::Acquire) {
        s.status = "blocked".into();
        s.last_error = Some(
            "tool_worker_unresolved: review external write outcomes and restart before another run"
                .into(),
        );
        s.finish_maintenance();
        s.finish_run();
        return s;
    }
    if s.question.is_some() {
        consume_commands(&mut s, &mut commands);
        match question::run(
            s,
            client.clone(),
            cancel.clone(),
            events.clone(),
            started,
            initial_tokens,
            &mut commands,
        )
        .await
        {
            question::Outcome::Answered(answered) => return *answered,
            question::Outcome::Work(work) => s = *work,
        }
    }
    s.begin_run();
    tools::document_review::refresh_policy(&mut s);
    s.task.migrate_legacy_plan();
    s.status = "running".into();
    s.last_error = None;
    // Every run brings its own budget. A resumed task starts a fresh progress
    // ladder; closing mode and its reported gaps belong to the previous run.
    s.progress_recovery.closing = None;
    s.progress_recovery.evidence_credit = s.sources.len();
    s.progress_recovery.best_score = progress_score(&s);
    s.progress_recovery.rounds_since_best = 0;
    // The ladder counts completed model requests, so the first loop pass of
    // a run only establishes the baseline score.
    let mut ladder_request_completed = false;
    s.completion_gaps.clear();
    let mut failure = None;
    let mut finalization_attempts = s.progress_recovery.finalization_attempts;
    let mut length_recoveries = 0usize;
    let mut empty_completions = 0usize;
    let mut review_response_failures = 0usize;
    // The final answer that started the pending document review, with the
    // output hash it was given for. An approval of that same document resumes
    // it instead of asking the model to answer again: the repeated request
    // cost a full-context round per approval (8% of a live run's input).
    let mut held_final: Option<(String, Option<String>)> = None;
    // Consecutive provider outages and the requests they consumed.
    let mut provider_outages = 0usize;
    let mut provider_requests = 0usize;
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
        consume_commands(&mut s, &mut commands);
        match apply_pending_config(&mut s, true) {
            Ok(true) => {
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
            Ok(false) => {}
        }
        // Final answers, model errors and cancelled runs can bypass the tool
        // batch checks. Recheck before every request, including resumed runs.
        if let Err(error) = s.check_runtime_capacity() {
            failure = Some(error.to_string());
            break;
        }
        if started.elapsed().as_secs() >= s.config.run_timeout_secs
            || s.input_tokens
                .saturating_add(s.output_tokens)
                .saturating_sub(initial_tokens)
                >= s.config.run_tokens
        {
            let reason = if started.elapsed().as_secs() >= s.config.run_timeout_secs {
                "run_timeout"
            } else {
                "run_budget_exhausted"
            };
            if let Some(text) = force_finish(&mut s, reason) {
                s.set_run_stop_reason(reason);
                publish_final(&mut s, text, &events, &cancel, started).await;
                break;
            }
            s.set_run_stop_reason(reason);
            failure = Some(format!("{reason}: partial results and memory retained"));
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
            && s.task.workflow != "source_document"
            && s.task.current_todo().is_none()
            && s.task.deliverables.is_empty()
            && !s.document_written
        {
            phase = "answer".into();
        }
        // Six rounds without a saved document steer this request toward
        // drafting. The nudge follows from state on every request and is not
        // persisted: a persisted draft outlived the first save, so reading for
        // later sections never counted as progress (a live run reached 15/24).
        let undrafted_phase = phase.clone();
        let drafting_nudge = s.task.workflow == "source_document"
            && !s.document_written
            && s.task_rounds >= 6
            && phase != "verify";
        if drafting_nudge {
            phase = "draft".into();
        }
        // A completion repair stays open from a rejection until the next
        // review, even after repair work changes the result: a live run's
        // first repair read dropped the unmet checks, the repair guidance
        // and the evidence credit, and the run drifted into a stall. The
        // session error, shown to the user, still ends with the rejection of
        // this very result; run_guidance keeps the repair message below.
        let completion_repair = tools::completion_review::open_repair(&s);
        let completion_repair_open = completion_repair.is_some();
        if completion_repair != Some(tools::completion_review::OpenRepair::Current)
            && s.last_error
                .as_deref()
                .is_some_and(|error| error.starts_with("completion_review_unmet:"))
        {
            s.last_error = None;
        }
        if finalization_attempts > 0
            || (s.config.source_document_review
                && tools::document_review::rejected_on_current_result(&s))
            || completion_repair_open
        {
            // A rejected document completion always returns to verification,
            // even if the model previously declared itself ready to answer.
            phase = "verify".into();
        }
        // One progress ladder for document work: focus (1x), narrowed tools
        // (2x), then closing mode (3x) or the closing budget reserve. Review
        // pages only count when their response had to be retried.
        let unread_count = unread_citation_count(&s);
        let review_request =
            s.checkpoint.is_none() && (s.completion_review.pending || s.document_review.pending);
        let counts_as_round = std::mem::replace(&mut ladder_request_completed, true)
            && (!review_request || review_response_failures > 0);
        // A closing request counted here may still become a checkpoint
        // request below; that request then returns its closing round.
        let mut closing_round_counted = false;
        if s.checkpoint.is_none() {
            // Reading new sources is progress until the document exists, even
            // after the budget or the document workflow switches the
            // guidance to drafting; afterwards only result improvements count,
            // except while review findings are open: repairing them needs new
            // evidence, and a live run stalled out reading exactly that.
            let review_repair_open = (s.config.source_document_review
                && tools::document_review::rejected_on_current_result(&s))
                || completion_repair_open;
            if phase == "investigate"
                || !s.document_written
                || review_repair_open
                || unread_count > 0
            {
                s.progress_recovery.evidence_credit =
                    s.progress_recovery.evidence_credit.max(s.sources.len());
            }
            let score = progress_score(&s);
            if score > s.progress_recovery.best_score {
                s.progress_recovery.best_score = score;
                s.progress_recovery.rounds_since_best = 0;
            } else if counts_as_round {
                s.progress_recovery.rounds_since_best =
                    s.progress_recovery.rounds_since_best.saturating_add(1);
            }
            if s.is_document_work() && s.progress_recovery.closing.is_none() {
                let reason = if fraction <= s.config.closing_reserve_ratio {
                    Some("budget")
                } else if s.progress_recovery.rounds_since_best >= closing_stall_limit(&s.config) {
                    Some("stall")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    s.progress_recovery.closing = Some(crate::session::Closing {
                        reason: reason.into(),
                        ..Default::default()
                    });
                    emit(
                        &events,
                        AgentEvent::Notice {
                            session: s.id.clone(),
                            text: format!(
                                "{} 수집한 근거로 문서를 마무리하고, 확인하지 못한 항목은 결과에 명시합니다.",
                                if reason == "budget" {
                                    "마감 예산에 도달해 마감 단계로 전환합니다."
                                } else {
                                    "진행이 오래 멈춰 마감 단계로 전환합니다."
                                }
                            ),
                        },
                        &cancel,
                        run_deadline(started, &s.config),
                    )
                    .await;
                }
            }
            if let Some(closing) = &mut s.progress_recovery.closing {
                if counts_as_round {
                    closing.rounds = closing.rounds.saturating_add(1);
                    closing_round_counted = true;
                }
                if closing.rounds > CLOSING_ROUND_LIMIT {
                    if let Some(text) = force_finish(&mut s, "closing_round_limit") {
                        publish_final(&mut s, text, &events, &cancel, started).await;
                        break;
                    }
                    failure = Some(
                        "closing_round_limit: closing requests were spent before a document was saved; gathered evidence and memory retained"
                            .into(),
                    );
                    break;
                }
            }
        }
        // Progress recovery steers only this request. Persisting its verify
        // would ratchet the task into verification for good: a live run was
        // left unable to register its next section after one early stall.
        let persisted_phase = if drafting_nudge && phase == "draft" {
            undrafted_phase
        } else {
            phase.clone()
        };
        let closing_active = s.progress_recovery.closing.is_some();
        if closing_active {
            phase = "verify".into();
        }
        let stall_rounds = s.progress_recovery.rounds_since_best;
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
                || (document_work && stall_rounds >= s.config.stall_round_limit)
                || rounds_without_progress >= s.config.stall_round_limit
                || focused_repair
                || repeated_outcome_focus
                || substantive_focus
                || artifact_focus);
        if progress_recovery {
            phase = if document_work {
                if phase == "verify" || s.document_written {
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
        if closing_active {
            phase = "verify".into();
        }
        // The answer workflow has no drafting or verification stage: a low
        // budget means answering from the evidence already gathered.
        let answer_workflow = s.task.workflow == "answer";
        if answer_workflow && matches!(phase.as_str(), "verify" | "draft") {
            phase = "answer".into();
        }
        s.task.phase = if closing_active {
            phase.clone()
        } else {
            persisted_phase
        };
        let focused_instruction = if answer_workflow {
            ANSWER_FOCUSED_INSTRUCTION
        } else {
            DOCUMENT_FOCUSED_INSTRUCTION
        };
        const DOCUMENT_FOCUSED_INSTRUCTION: &str = "Focused recovery: choose the first document_review issue, unmet completion check or current to-do and perform one concrete action that changes the requested result or verifies specific missing evidence. Read recovery_reason and the last tool's recovery contract; correct the cause or choose a different action before retrying. A task_plan applied=false or unchanged=true result did no work. Do not submit another final answer with unfinished work, cycle between earlier file versions, merely rewrite the plan, or save another summary. After a real edit, advance its to-do or read any cited range it left unread. If the original result already exists, check it once with document_audit, then complete only the actual remaining work. Document retry counts are recovery signals, not permission to stop or weaken requirements: continue to final verification within the remaining tokens and time.";
        const ANSWER_INVESTIGATE_INSTRUCTION: &str = "For a SOURCE CODE question, the first batch should locate the requested symbols/routes with source_search or code_outline scoped to the named files. Batch independent searches or reads together instead of paying a model round per file. Do not begin with file_read of each file from line 1; that often misses the target and requires another read. After locating the branch, file_read only its relevant range with explicit start_line and max_lines, or use symbol_read. For an existing-document summary, read the relevant document sections directly. For a requested document edit, inspect the target section and make a targeted edit. Answer once evidence is sufficient.";
        const ANSWER_FOCUSED_INSTRUCTION: &str = "Focused recovery: choose the current to-do, or the question itself, and perform one concrete action that changes the requested result. Read recovery_reason and the last tool's recovery contract; correct the cause or choose a different action before retrying. A task_plan applied=false or unchanged=true result did no work. Do not submit another final answer with unfinished work, cycle between earlier file versions, merely rewrite the plan, or save another summary. If the requested result already exists, complete only the actual remaining work, then answer.";
        s.run_guidance = json!({"task_rounds":s.task_rounds,"run_rounds":s.run_rounds(),"finalization_attempts":finalization_attempts,"phase":phase,"remaining_tokens":remaining,"remaining_seconds":seconds_remaining,"unread_citation_count":unread_count,
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
            "completion_error":s.last_error.as_deref().filter(|error| finalization_attempts > 0 || error.starts_with("task_plan_pending:") || error.starts_with("completion_review")).or((completion_repair_open && s.checkpoint.is_none()).then_some(tools::completion_review::UNMET_ERROR)),
            "instruction":if focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus { focused_instruction } else if progress_recovery { if document_work { "Progress recovery: repeated investigation has not changed the document or resolved an unread citation. Use the evidence already gathered to make one small, safe document_edit now, or file_read one cited range reported as unread. Inspect the output hash if needed. Read only a specific missing source range that directly blocks that action. Do not gather more general evidence or save another memory first. If a claim cannot be supported, mark that gap in the relevant section and continue with supported work; do not invent evidence. A checkpoint remains the only exception for memory maintenance." } else if planned_work { "Progress recovery: plan edits or repeated reads have not produced an outcome. Execute the first unfinished item using available evidence and tools. Do not recreate the plan or save another summary. Insert only a concrete missing prerequisite; complete an item only with the actual result. If evidence is missing, read only the necessary range." } else { "Progress recovery: repeated preparation has not produced an outcome. Correct any necessary task_plan call using its returned example, then carry out the first concrete action; otherwise answer from existing evidence. Do not repeat an unchanged call or save another summary." } } else { match phase.as_str() {"answer"=>"Answer the user now from gathered evidence. Read further only for a concrete missing fact required by the question. Do not save memory before answering a simple explanation. State any missing coverage instead of claiming exhaustive review.","verify"=>"Stop expanding scope. For source documentation, batch targeted reads for missing evidence, then repair known issues in their original locations with targeted section or text edits when safe. Review findings are edit instructions, not document content: do not append a review, checks, improvements, or TODO section unless the user explicitly requested it. If a fact remains unverified, qualify it where the relevant claim appears; include a limitation only when needed for the requested document. Inspect the final outline for review-note headings before completion. Use document_edit_batch for related edits from one document snapshot; its operations are applied in order. Only requests containing document_edit or document_edit_batch advance the review interval (one per request, including failed edits); reads and verification do not. The runtime reviews after an executed edit batch reaches the interval, preserving all sibling calls. Unchanged rejected documents reuse their findings. This interval is not a total edit allowance: keep correcting the original requirements within the remaining run tokens and time. For questions or existing-document summaries, answer from the content already read and report any missing coverage.","draft"=>"For source documentation, save each investigated section in a separate edit as soon as its evidence is ready. Check the current outline; use insert_before/insert_after for siblings and insert_first_child/insert_last_child for nested sections when that preserves the document flow. Copy section_path when headings repeat; preserve verification budget. For questions or existing-document summaries, finish the chat answer using targeted reads only.",_=>if answer_workflow { ANSWER_INVESTIGATE_INSTRUCTION } else { "For source documentation, investigate one section, save it, then move to the next. Create only a short opening with the first ready section; inspect the outline before each later addition, copy section_path when headings repeat, and use sibling or child insertion to place it within the hierarchy. For a SOURCE CODE question, the first batch should locate the requested symbols/routes with source_search or code_outline scoped to the named files. Batch independent searches or reads together instead of paying a model round per file. Do not begin with file_read of each file from line 1; that often misses the target and requires another read. After locating the branch, file_read only its relevant range with explicit start_line and max_lines, or use symbol_read. For an existing-document summary, read the relevant document sections directly. Answer once evidence is sufficient; no source audit or document write is required." }} }});
        s.run_guidance["progress_recovery"]["rounds_since_progress"] = json!(stall_rounds);
        s.run_guidance["progress_recovery"]["closing_after"] =
            json!(closing_stall_limit(&s.config));
        if answer_workflow {
            // Drafting/verification reserves, document repair counts and
            // closing belong to document work, which answer never enters.
            for key in [
                "writing_reserve_tokens",
                "verification_reserve_tokens",
                "document_repair_limit",
                "document_repair_requests_used",
                "document_repair_requests_remaining",
                "pending_count",
            ] {
                s.run_guidance.as_object_mut().unwrap().remove(key);
            }
            s.run_guidance["progress_recovery"]
                .as_object_mut()
                .unwrap()
                .remove("closing_after");
        }
        if s.progress_recovery.closing.is_none() && ready_for_final(&mut s) {
            s.run_guidance["ready_for_final"] = json!(true);
            let previous = if s.config.source_document_review {
                s.document_review.issues.len()
            } else {
                0
            };
            let mut instruction = if previous > 0 {
                format!(
                    "{READY_FOR_FINAL_INSTRUCTION} document_review.issues contains {previous} findings from a previous document version. They are repair context, not confirmed defects in the current version. The next review will check the current document after your final answer."
                )
            } else {
                READY_FOR_FINAL_INSTRUCTION.to_owned()
            };
            // Closing the repair to-dos makes the run ready again; a live run
            // then answered without having repaired anything, and the next
            // review rejected the unchanged outcome.
            if let Some(prior) = tools::completion_review::prior_rejection(&s) {
                instruction.push_str(&format!(
                    " completion_review.last_review lists {} unmet check(s) from the completion review of an earlier version. If the current result does not address one of them yet, repair it first; state a check that cannot be met as a limitation in the final answer.",
                    prior.checks.len()
                ));
            }
            settle_completion_error(&mut s);
            s.run_guidance["instruction"] = json!(instruction);
        } else if s.progress_recovery.closing.is_none()
            && s.task.current_todo().is_some()
            && ready_except_plan(&mut s)
        {
            s.run_guidance["plan_closeout"] = plan_closeout(&s);
            s.run_guidance["instruction"] = json!(PLAN_CLOSEOUT_INSTRUCTION);
        } else if s.progress_recovery.closing.is_none()
            && s.run_guidance["document_readiness"]["structural_ok"] == false
        {
            s.run_guidance["instruction"] = json!(
                "The document is not ready for final acceptance. Resolve document_readiness.issues using their next actions: file_read each unread_citation range (or narrow that citation to the lines already read), or repair the reported format/citation problem. Do not submit another final answer before those issues are resolved. For additional issues, use document_audit with offset=next_offset and expected_revision=revision from this audit."
            );
        } else if s.progress_recovery.closing.is_none() && review_repair_pending(&s) {
            s.run_guidance["review_repair"] = json!({"findings":s.document_review.issues.len()});
            s.run_guidance["instruction"] = json!(REVIEW_REPAIR_INSTRUCTION);
            // After a checkpoint cleared the repair context, restate the
            // findings until the document changes.
            let resume = s.progress_recovery.review_repair_resume_hash.clone();
            if resume.is_some() && resume == output_hash(&s) {
                s.run_guidance["review_repair"]["resumed_after_checkpoint"] = json!(true);
                s.run_guidance["review_repair"]["unrepaired_findings"] =
                    json!(s.document_review.issues);
                s.run_guidance["instruction"] = json!(format!(
                    "{REVIEW_REPAIR_RESUME_INSTRUCTION} {REVIEW_REPAIR_INSTRUCTION}"
                ));
            } else if resume.is_some() {
                s.progress_recovery.review_repair_resume_hash = None;
            }
        }
        if let Some(closing) = &s.progress_recovery.closing {
            s.run_guidance["closing"] = json!({"active":true,"reason":closing.reason,
                "rounds":closing.rounds,"round_limit":CLOSING_ROUND_LIMIT,
                "final_attempts":closing.final_attempts});
            // Closing asks for the final answer and reports what stays
            // unmet; a rejection of an earlier version must not tell the
            // model not to answer. A rejection of this very result keeps it.
            if completion_repair != Some(tools::completion_review::OpenRepair::Current) {
                settle_completion_error(&mut s);
            }
            // Closing mode kept auditing a finished result for its whole
            // allowance in live runs; once nothing is left, say so.
            s.run_guidance["instruction"] = json!(if ready_for_final(&mut s) {
                s.run_guidance["ready_for_final"] = json!(true);
                format!(
                    "{READY_FOR_FINAL_INSTRUCTION} Closing mode is active: this is the time to answer."
                )
            } else {
                closing_instruction(&s)
            });
        }
        let definitions = ToolRegistry::definitions(&s);
        let mut request = match ContextManager::request(&s, definitions.clone()) {
            Ok(r) => r,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        let estimate = context::count(&request, &s.config.model);
        let checkpoint_was_open = s.checkpoint.is_some();
        if let Err(e) = ContextManager::prepare(&mut s, estimate) {
            failure = Some(e.to_string());
            break;
        }
        // Checkpoint requests do not count as closing rounds, so the one that
        // just started must not spend the round counted above either:
        // otherwise the last closing request becomes cleanup and the run is
        // force-finished right after the checkpoint completes.
        if closing_round_counted
            && !checkpoint_was_open
            && s.checkpoint.is_some()
            && let Some(closing) = &mut s.progress_recovery.closing
        {
            closing.rounds = closing.rounds.saturating_sub(1);
        }
        if let Some(cp) = &mut s.checkpoint {
            let (max_requests, max_failures) = if document_work {
                (
                    context::DOCUMENT_CHECKPOINT_MAX_REQUESTS,
                    context::DOCUMENT_CHECKPOINT_MAX_FAILURES,
                )
            } else {
                (
                    context::CHECKPOINT_MAX_REQUESTS,
                    context::CHECKPOINT_MAX_FAILURES,
                )
            };
            if cp.attempts >= max_requests || cp.failed_attempts >= max_failures {
                failure = Some(format!(
                    "checkpoint_retry_limit: {} of {} requests, {} of {} failed requests; last cause: {}; original context retained",
                    cp.attempts,
                    max_requests,
                    cp.failed_attempts,
                    max_failures,
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
        // Any other request means the held answer no longer ends the run.
        if !reviewing_document {
            held_final = None;
        }
        let mut replaying_final = false;
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
        // After an empty reply, let the model choose: some providers answer a
        // required tool choice with reasoning only and no content or calls.
        if document_work
            && s.progress_recovery.action_required
            && empty_completions == 0
            && s.checkpoint.is_none()
            && !reviewing_completion
            && !reviewing_document
        {
            // A rejected final must return to a concrete tool action. Review
            // requests remain tool-free, and a subsequent final is still gated.
            request["tool_choice"] = json!("required");
        }
        let buffer_answer = reviewing_completion || reviewing_document;
        let raw_request_tokens = context::count(&request, &s.config.model);
        let request_tokens = ContextManager::calibrated(&s, raw_request_tokens);
        // Reasoning models spend output tokens on reasoning too. Cleanup
        // requests use the bounded cleanup allowance that input_budget reserves.
        let mut request_config = s.config.clone();
        if s.checkpoint.is_some() {
            request_config.output_tokens = ContextManager::cleanup_output_tokens(&s.config);
        }
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
            if let Some(text) = force_finish(&mut s, "run_budget_exhausted") {
                publish_final(&mut s, text, &events, &cancel, started).await;
                break;
            }
            failure = Some("run_budget_exhausted: insufficient budget for next request".into());
            break;
        }
        s.task_rounds += 1;
        s.activity = json!({"stage":if reviewing_completion {"completion_review"} else if reviewing_document {"document_review"} else {"model"},"started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds()});
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
        let invalid_completion = response
            .as_ref()
            .ok()
            .and_then(|completion| crate::llm::validate_completion_bounds(completion).err());
        let response_error = response.as_ref().err().or(invalid_completion.as_ref());
        let recovered_batch = response_error
            .is_some_and(|error| recover_unexecuted_batch(&mut s, &error.to_string()));

        // Attribute every charged request before any retry or stop can leave
        // this iteration, including failed provider calls and invalid output.
        match &response {
            Err(error) => {
                // A refusal with an HTTP status (429, 400, 5xx) was not billed;
                // estimate only attempts that may have generated tokens.
                let attempts = error
                    .downcast_ref::<crate::llm::CompletionError>()
                    .map_or(1, crate::llm::CompletionError::billable_attempts);
                if attempts > 0 {
                    s.note_run_estimate();
                    s.usage_incomplete = true;
                }
                s.input_tokens = s
                    .input_tokens
                    .saturating_add(request_tokens.saturating_mul(attempts));
                if recovered_batch {
                    // Parsing failures have no trusted usage receipt. Charge
                    // the reserved output as well as every attempted input.
                    s.output_tokens = s
                        .output_tokens
                        .saturating_add(request_config.output_tokens.saturating_mul(attempts));
                }
            }
            Ok(completion) => {
                provider_outages = 0;
                provider_requests = 0;
                let extra_attempts = completion.billable_failed_attempts();
                if extra_attempts > 0 {
                    s.note_run_estimate();
                    s.usage_incomplete = true;
                    s.input_tokens = s
                        .input_tokens
                        .saturating_add(request_tokens.saturating_mul(extra_attempts));
                }
                if let Some(usage) = &completion.usage {
                    if invalid_completion.is_none() && completion.attempts <= 1 {
                        ContextManager::record_usage(&mut s, raw_request_tokens, usage.input);
                    }
                    s.input_tokens = s.input_tokens.saturating_add(usage.input);
                    s.output_tokens = s.output_tokens.saturating_add(usage.output);
                    if let Some(cached) = usage.cached {
                        s.cached_tokens = Some(s.cached_tokens.unwrap_or(0).saturating_add(cached));
                    }
                } else {
                    s.note_run_estimate();
                    s.usage_incomplete = true;
                    s.input_tokens = s.input_tokens.saturating_add(request_tokens);
                    s.output_tokens = s.output_tokens.saturating_add(
                        if invalid_completion.is_some() || completion.length_limited {
                            // Invalid output or invisible reasoning can consume
                            // the whole allowance without a trusted receipt.
                            request_config.output_tokens
                        } else {
                            context::tokens(&completion.text, &s.config.model)
                                + completion
                                    .calls
                                    .iter()
                                    .map(|c| context::tokens(&c.arguments, &s.config.model))
                                    .sum::<usize>()
                        },
                    );
                }
            }
        }
        let charged_input = s.input_tokens.saturating_sub(usage_before.0);
        let charged_output = s.output_tokens.saturating_sub(usage_before.1);
        if reviewing_completion {
            s.completion_review.input_tokens = s
                .completion_review
                .input_tokens
                .saturating_add(charged_input);
            s.completion_review.output_tokens = s
                .completion_review
                .output_tokens
                .saturating_add(charged_output);
        }
        if reviewing_document {
            s.document_review.input_tokens =
                s.document_review.input_tokens.saturating_add(charged_input);
            s.document_review.output_tokens = s
                .document_review
                .output_tokens
                .saturating_add(charged_output);
        }
        let response = match invalid_completion {
            Some(error) => Err(error),
            None => response,
        };
        let mut completion = match response {
            Ok(completion) => completion,
            Err(error) => {
                if recovered_batch {
                    if reviewing_completion || reviewing_document {
                        review_response_failures += 1;
                        if abandon_failing_review(&mut s, review_response_failures).is_some() {
                            review_response_failures = 0;
                        }
                    }
                    continue;
                }
                // The client's quick retries cover a blip, not an outage of a
                // few minutes; a live run lost 3M tokens of work to one 502.
                if let Some(provider) = error
                    .downcast_ref::<crate::llm::CompletionError>()
                    .filter(|provider| provider.provider_unavailable())
                {
                    provider_requests = provider_requests.saturating_add(provider.attempts());
                    let wait = PROVIDER_OUTAGE_WAITS
                        .get(provider_outages)
                        .map(|&secs| Duration::from_secs(secs));
                    if let Some(wait) = wait
                        && tokio::time::Instant::now() + wait < deadline
                    {
                        provider_outages += 1;
                        // A request that never answered is not a checkpoint
                        // attempt, a task round or a progress-ladder round;
                        // the resent request takes its place.
                        if let Some(cp) = &mut s.checkpoint {
                            cp.attempts = cp.attempts.saturating_sub(1);
                        }
                        s.task_rounds = s.task_rounds.saturating_sub(1);
                        ladder_request_completed = false;
                        emit(
                            &events,
                            AgentEvent::Notice {
                                session: s.id.clone(),
                                text: format!(
                                    "Model provider unavailable; retrying in {}s ({}/{})",
                                    wait.as_secs(),
                                    provider_outages,
                                    PROVIDER_OUTAGE_WAITS.len()
                                ),
                            },
                            &cancel,
                            deadline,
                        )
                        .await;
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = tokio::time::sleep(wait) => {}
                        }
                        continue;
                    }
                    failure = Some(format!(
                        "{error}; provider unavailable after {provider_requests} requests over {} waits",
                        provider_outages
                    ));
                    break;
                }
                failure = Some(error.to_string());
                break;
            }
        };
        if reviewing_completion {
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
                    if let Some(notice) = abandon_failing_review(&mut s, review_response_failures) {
                        review_response_failures = 0;
                        emit(
                            &events,
                            AgentEvent::Notice {
                                session: s.id.clone(),
                                text: notice.into(),
                            },
                            &cancel,
                            run_deadline(started, &s.config),
                        )
                        .await;
                        snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                        continue;
                    }
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
            // Validate the visible response before looking at provider
            // metadata. Some compatible providers attach a spurious tool call
            // or report finish_reason=length even when the JSON object is
            // complete. The review request has no executable tools, so a
            // complete, hash-checked verdict is safe to accept in that case.
            // A reasoning model can spend the bounded first-attempt allowance
            // before the JSON closes. Name that cause instead of tool use, so
            // the retry shortens its verdict rather than looking for tools.
            let truncated = || {
                anyhow::anyhow!(
                    "document_review_incomplete: response reached the output token limit before the JSON closed; return one complete issues JSON object with fewer, shorter issues and quotes"
                )
            };
            let review_result = if completion.discarded_tool_calls || !completion.calls.is_empty() {
                Err(anyhow::anyhow!(
                    "document_review_incomplete: review must return complete JSON without tools"
                ))
            } else if completion.text.trim().is_empty() {
                Err(if completion.length_limited {
                    truncated()
                } else {
                    anyhow::anyhow!(
                        "document_review_incomplete: review returned no text; return complete issues JSON"
                    )
                })
            } else {
                // Mirror abandon_failing_review: this response is the last
                // one before a page skip or, in closing, an unreviewed result.
                let last_try = s.checkpoint.is_none()
                    && (s.progress_recovery.closing.is_some()
                        || review_response_failures + 1 >= REVIEW_UNAVAILABLE_LIMIT);
                let finish = if last_try {
                    tools::document_review::finish_last_try
                } else {
                    tools::document_review::finish
                };
                match finish(&mut s, &completion.text) {
                    Ok(()) => Ok(()),
                    Err(error)
                        if completion.length_limited
                            && error.to_string().starts_with("document_review_invalid:") =>
                    {
                        Err(truncated())
                    }
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = review_result {
                let reason = error.to_string();
                review_response_failures += 1;
                s.last_error = Some(reason.clone());
                if (reason.starts_with("document_review_invalid:")
                    || reason.starts_with("document_review_incomplete:")
                    || reason.starts_with("document_review_stale:"))
                    && let Some(notice) = abandon_failing_review(&mut s, review_response_failures)
                {
                    review_response_failures = 0;
                    emit(
                        &events,
                        AgentEvent::Notice {
                            session: s.id.clone(),
                            text: notice.into(),
                        },
                        &cancel,
                        run_deadline(started, &s.config),
                    )
                    .await;
                    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
                    continue;
                }
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
            note_document_review_verdict(&mut s);
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            let held = held_final
                .take()
                .filter(|_| !s.document_review.pending && tools::document_review::approved(&s))
                .filter(|(_, hash)| *hash == output_hash(&s));
            let Some((text, _)) = held else {
                continue;
            };
            // Re-enter the final-answer checks with the answer that started
            // this review; a rejection there returns to the model as before.
            completion.text = text;
            completion.calls.clear();
            completion.discarded_tool_calls = false;
            completion.length_limited = false;
            replaying_final = true;
            s.last_error = None;
        }
        if !completion.length_limited
            && completion.calls.is_empty()
            && completion.text.trim().is_empty()
        {
            // An empty reply is usually transient (the provider stopped before
            // producing content). Ask again a bounded number of times before
            // stopping; this also covers work not yet classified as a document.
            empty_completions += 1;
            // Change the next request: an identical one tends to produce the
            // same empty reply again. recovery_reason reaches run_guidance.
            if recover_document(
                &mut s,
                &format!(
                    "The model returned {empty_completions} empty response(s) in a row (no text and no tool call). Call one concrete document repair or verification tool, or give the final answer if the document is finished."
                ),
            ) {
                s.last_error = Some(format!(
                    "empty_completion: {empty_completions} consecutive responses had neither text nor tool calls"
                ));
                s.progress_recovery.action_required = true;
                if empty_completions >= EMPTY_DOCUMENT_CLOSING
                    && s.progress_recovery.closing.is_none()
                {
                    s.progress_recovery.closing = Some(crate::session::Closing {
                        reason: "empty_response".into(),
                        ..Default::default()
                    });
                    let next_step = if document_saved(&s) {
                        "저장된 문서로 마무리하고, 확인하지 못한 항목은 결과에 명시합니다."
                    } else {
                        "수집한 근거로 문서 생성을 재시도합니다. 문서가 저장되지 않으면 미완료로 종료합니다."
                    };
                    emit(
                        &events,
                        AgentEvent::Notice {
                            session: s.id.clone(),
                            text: format!(
                                "모델이 빈 응답을 반복해 마감 단계로 전환합니다. {next_step}"
                            ),
                        },
                        &cancel,
                        run_deadline(started, &s.config),
                    )
                    .await;
                }
                continue;
            }
            if empty_completions <= EMPTY_COMPLETION_RETRIES {
                s.last_error = Some(
                    "empty_completion: the previous response had neither text nor tool calls; continue the task with a tool call or the answer".into(),
                );
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

        empty_completions = 0;
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
            // Rewriting a grown document in one call is the usual way to exceed
            // the output limit; require section-sized edits. A document small
            // relative to the limit cannot be the cause, so it stays writable.
            let document_tokens = tools::output_path(&s.project)
                .and_then(|path| tools::read_text(&path))
                .ok()
                .map_or(0, |doc| context::tokens(&doc, &s.config.model));
            if s.is_document_work()
                && s.document_written
                && document_tokens >= s.config.output_tokens / 4
            {
                s.progress_recovery.whole_write_withheld = true;
            }
            s.activity = json!({"stage":"continuing","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds()});
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            if length_recoveries >= LENGTH_RECOVERY_LIMIT {
                let reason = format!(
                    "length_recovery_limit: {length_recoveries} consecutive output-limit responses. Split document edits into smaller complete calls (one section per document_edit action=section or replace_text; whole-document write is withheld) and keep final reports concise; discarded tool batches did not execute."
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
            let hold_document_final = document_workflow && s.checkpoint.is_none();
            if buffer_answer && !hold_document_final {
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
                let reason = "Pending settings still require cleanup; use the current settings to finish the cleanup and apply the pending update before the final answer";
                if s.is_document_work() {
                    s.last_error = Some(reason.into());
                    s.progress_recovery.action_required = true;
                    recover_document(&mut s, reason);
                    continue;
                }
                failure = Some(reason.into());
                break;
            }
            // Closing mode gives a rejected final one repair turn. The next
            // final is accepted and every unresolved item is reported instead.
            let closing_attempt = if reviewing_completion || replaying_final {
                s.progress_recovery
                    .closing
                    .as_ref()
                    .map(|c| c.final_attempts)
            } else {
                s.progress_recovery.closing.as_mut().map(|closing| {
                    closing.final_attempts = closing.final_attempts.saturating_add(1);
                    closing.final_attempts
                })
            };
            let accept_gaps = s.is_document_work()
                && closing_attempt.is_some_and(|n| n >= 2)
                && document_saved(&s);
            let mut waived = Vec::new();
            if !accept_gaps && let Some(item) = s.task.current_todo() {
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
            if let Err(error) = tools::verify_document_write(&s) {
                s.status = "partial".into();
                s.last_error = Some(error.to_string());
            } else if s.is_document_work() && !s.document_written {
                s.status = "partial".into();
                s.last_error = Some("The requested source document has not been saved in this task: create it with document_edit (action=create) from the evidence already gathered before finishing".into());
            } else if s.is_document_work() {
                let _ = tools::revalidate(&mut s);
                match tools::audit_document(&mut s) {
                    Ok(audit) if audit["structural_ok"] == true => {
                        let closing_review_used = s
                            .progress_recovery
                            .closing
                            .as_ref()
                            .is_some_and(|c| c.document_review_used);
                        // A document that cites no source has nothing
                        // to compare; the review is skipped, not failed.
                        if s.document_written
                            && s.config.source_document_review
                            && tools::citation_count(&s) > 0
                            && !tools::document_review::approved(&s)
                            && !tools::document_review::unavailable_on_current(&s)
                            && !closing_review_used
                        {
                            // Closing mode spends at most one review; its
                            // unresolved findings are reported afterwards.
                            if let Some(closing) = &mut s.progress_recovery.closing {
                                closing.document_review_used = true;
                            }
                            if tools::document_review::rejected_on_current_result(&s) {
                                s.status = "partial".into();
                                s.last_error = Some(format!(
                                    "document_review: unchanged document still requires correction: {}",
                                    s.document_review.issues.join("; ")
                                ));
                                // Repeating the final answer without editing the
                                // rejected document is not repair. After a second
                                // unchanged rejection, close instead of waiting for
                                // the stall ladder; the next final is accepted with
                                // the findings reported. One review stays available:
                                // an edit made while closing is reviewed again, an
                                // unchanged document reuses the rejection.
                                if s.progress_recovery.closing.is_none()
                                    && note_unrepaired_final(&mut s) >= UNREPAIRED_FINAL_LIMIT
                                {
                                    s.progress_recovery.closing = Some(crate::session::Closing {
                                        reason: "review_unrepaired".into(),
                                        final_attempts: 1,
                                        ..Default::default()
                                    });
                                    emit(&events, AgentEvent::Notice { session: s.id.clone(), text: "검토 지적을 반영하지 않은 채 최종 답변이 반복돼 마감 단계로 전환합니다. 남은 지적은 결과에 명시합니다.".into() }, &cancel, run_deadline(started, &s.config)).await;
                                }
                            } else {
                                s.document_review.pending = true;
                                s.status = "running".into();
                                held_final = (!continuing)
                                    .then(|| (completion.text.clone(), output_hash(&s)));
                                emit(&events, AgentEvent::Notice { session:s.id.clone(), text:"Reviewing the document against source evidence and requested coverage.".into() }, &cancel, run_deadline(started, &s.config)).await;
                                continue;
                            }
                        } else {
                            // Approved, or finishing without a further
                            // review: collect_gaps reports the latter.
                            s.status = "complete".into();
                            s.last_error = None;
                        }
                    }
                    Ok(audit) => {
                        s.status = "partial".into();
                        s.last_error = Some(format!(
                            "Document format/evidence audit has {} issues; use document_audit",
                            audit["issue_count"]
                        ));
                    }
                    Err(error) => {
                        s.status = "partial".into();
                        s.last_error = Some(format!("Document audit failed: {error}"));
                    }
                }
            } else {
                s.status = "complete".into();
                s.last_error = None;
            }
            let closing_acceptance_used = s
                .progress_recovery
                .closing
                .as_ref()
                .is_some_and(|c| c.completion_review_used);
            if s.status == "complete"
                && tools::completion_review::required(&s)
                && (!closing_acceptance_used || reviewing_completion)
            {
                match tools::completion_review::begin_final(&mut s, &completion.text, continuing) {
                    Ok(
                        tools::completion_review::Gate::Accepted
                        | tools::completion_review::Gate::Unavailable,
                    ) => {}
                    Ok(tools::completion_review::Gate::Review) => {
                        if let Some(closing) = &mut s.progress_recovery.closing {
                            closing.completion_review_used = true;
                        }
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
                    Ok(tools::completion_review::Gate::Repair) if accept_gaps => {}
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
                    Err(error) if accept_gaps => {
                        waived.push(format!("완료 조건 검증 — {error}"));
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
            // Before accepting, spend closing's one review on a document that
            // changed since its rejection, so a real fix is not reported as an
            // open finding. An unchanged document keeps the rejection.
            if s.status == "partial"
                && accept_gaps
                && s.document_written
                && s.config.source_document_review
                && !s.document_review.issues.is_empty()
                && !tools::document_review::approved(&s)
                && !tools::document_review::rejected_on_current_result(&s)
                && !tools::document_review::unavailable_on_current(&s)
                && s.progress_recovery
                    .closing
                    .as_ref()
                    .is_some_and(|closing| !closing.document_review_used)
            {
                if let Some(closing) = &mut s.progress_recovery.closing {
                    closing.document_review_used = true;
                }
                s.document_review.pending = true;
                s.status = "running".into();
                s.last_error = None;
                held_final = (!continuing).then(|| (completion.text.clone(), output_hash(&s)));
                emit(
                    &events,
                    AgentEvent::Notice {
                        session: s.id.clone(),
                        text: "Reviewing the corrected document once before closing.".into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                continue;
            }
            if s.status == "partial" && accept_gaps {
                // Second closing final: accept the saved document and report
                // everything unresolved rather than returning to repairs. Every
                // partial cause is re-derived from state by collect_gaps.
                s.last_error = None;
                s.status = "complete".into();
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
            let mut final_text = completion.text.clone();
            if s.status == "complete" && s.is_document_work() {
                s.completion_gaps = collect_gaps(&mut s, &waived);
                if !s.completion_gaps.is_empty() {
                    s.status = "complete_with_gaps".into();
                    let cause = s
                        .progress_recovery
                        .closing
                        .as_ref()
                        .map(|c| c.reason.clone());
                    final_text = gap_report(&s, Some(&completion.text), cause.as_deref());
                    message["content"] = json!(final_text);
                }
            }
            if hold_document_final && matches!(s.status.as_str(), "complete" | "complete_with_gaps")
            {
                s.history.push(vec![message], true);
                emit(
                    &events,
                    AgentEvent::Delta {
                        session: s.id.clone(),
                        text: final_text,
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
            }
            break;
        }
        if s.checkpoint.is_none() {
            s.progress_recovery.repair_step = tools::ToolRegistry::repair_only(&s);
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
        if (buffer_answer || document_workflow)
            && !s.is_document_work()
            && !completion.text.is_empty()
        {
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
        // Tool-call prose is intermediate. For document work it may claim
        // completion before the calls have run, so keep only the executable
        // calls in user-visible history; the final verified reply is added later.
        let history_text = if s.is_document_work() {
            ""
        } else {
            &completion.text
        };
        let mut messages = vec![assistant(history_text, &completion.calls)];
        // A checkpoint acknowledges the whole batch. Evaluate it after writes,
        // even if a provider emits its call first; preserve other write ordering.
        let mut execution_calls = completion.calls.clone();
        execution_calls.sort_by_key(|call| call.name == "checkpoint_complete");
        let mut i = 0;
        let mut checkpoint_failure_recorded = false;
        let mut remaining = batch_limit;
        let mut seen_call_ids = std::collections::BTreeSet::new();
        let mut batch_document_hash: Option<String> = None;
        let prior_source_count = s.sources.len();
        let prior_unread = unread_citation_count(&s);
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
            s.activity = json!({"stage":"tools","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds(),"tools":execution_calls[i..i+group].iter().map(|c| c.name.clone()).collect::<Vec<_>>()});
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            // A single completion can contain many groups. Stop before the
            // next group when earlier archives or receipts exhausted capacity.
            if failure.is_none()
                && let Err(error) = s.check_runtime_capacity()
            {
                failure = Some(error.to_string());
            }
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
                let deadline = run_deadline(started, &s.config);
                read_parallel(&mut s, &execution_calls[i..i + group], &cancel, deadline).await
            } else {
                let deadline = run_deadline(started, &s.config);
                let (next, result) = execute_one(s, effective_call, &cancel, deadline).await;
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
                if !replayed_call {
                    tool_failures.mark_repeated_failure(&call.name, &call.arguments, &mut result);
                }
                if let Some(reason) = tool_failures.observe(
                    &call.name,
                    &call.arguments,
                    &result,
                    s.config.stall_round_limit,
                ) && failure.is_none()
                {
                    if reason.starts_with("identical_tool_failure:") {
                        tools::recovery::annotate_identical_document_failure(&mut result);
                    }
                    let correctable = tools::recovery::correctable_document_error(&result);
                    if !correctable || !recover_document(&mut s, &reason) {
                        failure = Some(reason);
                    }
                }
                if result["error"].as_str().is_some_and(|e| {
                    tools::uncertain_write_error(e) || e.starts_with("run_timeout")
                }) {
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
                    "symbol_relations",
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
                let result = suppress_unchanged_repeat(&s, &messages, call, result);
                let result = compact_repair_audit(&s, call, result);
                tools::record_delivered_read(&mut s, call, &result);
                if result["status"] == "ok"
                    && [
                        "file_list",
                        "source_search",
                        "symbol_search",
                        "symbol_relations",
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
        // Reading a cited range the document still owed counts like a
        // verification did: the saved result is better supported.
        let verified_progress = unread_citation_count(&s) < prior_unread;
        if verified_progress {
            repetitions.clear();
        }
        let new_source_evidence = s.sources.len() > prior_source_count;
        let pending_todos = s.task.todos.iter().filter(|item| !item.done).count();
        let plan_advanced = prior_current_todo.is_some()
            && pending_todos < prior_pending_todos
            && s.task.current_todo().map(|item| &item.id) != prior_current_todo.as_ref();
        if verified_progress || plan_advanced || artifact_milestone {
            s.progress_recovery.artifact_edits_without_milestone = 0;
        } else if novel_artifact_change && !checkpoint_batch {
            s.progress_recovery.artifact_edits_without_milestone = s
                .progress_recovery
                .artifact_edits_without_milestone
                .saturating_add(1);
        }
        s.progress_recovery.repair_step = false;
        if !s.is_document_work() {
            // Outside document work the reason only explains the rejected batch.
            s.progress_recovery.recovery_reason = None;
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
        if novel_artifact_change || verified_progress || new_source_evidence {
            s.progress_recovery.rounds_without_substantive_progress = 0;
        } else if !checkpoint_batch {
            s.progress_recovery.rounds_without_substantive_progress = s
                .progress_recovery
                .rounds_without_substantive_progress
                .saturating_add(1);
        }
        if verified_progress || artifact_milestone || new_source_evidence {
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
        // Bound ancillary session data too; never silently discard observations or receipts.
        if let Err(error) = s.check_runtime_capacity() {
            failure = Some(error.to_string());
            break;
        }
        // Count the work since the last confirmed review improvement even when
        // new evidence has made that verdict stale. Changing results alone
        // must not reset this budget; stale checks cannot reopen repair to-dos.
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
                    "Repair rounds have not produced a new accepted result. Finish the concrete correction or targeted verification, then give the final answer to request a review of the current result.",
                )
            {
                tools::completion_review::schedule_repairs(&mut s);
                s.status = "partial".into();
                s.last_error = Some("completion_review_no_progress: repair rounds exhausted without a new approval; result and review history retained for re-evaluation".into());
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
    if s.write_outcome_uncertain.load(Ordering::Acquire) {
        s.status = "blocked".into();
        s.last_error = Some(failure.filter(|error| tools::uncertain_write_error(error)).unwrap_or_else(|| "tool_worker_unresolved: review external write outcomes and restart before another run".into()));
    } else if cancel.is_cancelled() {
        s.status = "cancelled".into();
        s.last_error = None;
    } else if let Some(error) = failure {
        s.status = "blocked".into();
        s.last_error = Some(error);
    }
    s.finish_maintenance();
    s.finish_run();
    s.activity = json!({"stage":"idle"});
    snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
    s
}

pub async fn headless(mut s: Session, prompt: String) -> Result<Session> {
    s.history.check_append(
        vec![json!({"role":"user","content":prompt})],
        s.config.history_bytes,
    )?;
    s.receive_message(prompt)?;
    s.check_runtime_capacity()?;
    let console = console::Console::new()?;
    let output = console.sender();
    let (tx, mut rx) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let listener = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        c.cancel();
    });
    let _listener_guard = AbortOnDrop(listener.abort_handle());
    let render_cancel = cancel.clone();
    let mut renderer = tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = render_cancel.cancelled() => break,
                event = rx.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            let (target, text) = match event {
                AgentEvent::Delta { text, .. } => (console::Target::Stdout, text),
                AgentEvent::Tool { name, status, .. } => {
                    (console::Target::Stderr, format!("\n[{name}: {status}]\n"))
                }
                _ => continue,
            };
            if !output.write(target, &text, &render_cancel).await {
                // A closed/erroring output pipe has no reader for this CLI
                // run. Release its provider request instead of running unseen.
                render_cancel.cancel();
                break;
            }
        }
    });
    let _renderer_guard = AbortOnDrop(renderer.abort_handle());
    let result = run_session(s, Arc::new(OpenAiClient), cancel, tx).await;
    listener.abort();
    let deadline = tokio::time::Instant::now() + console::DRAIN_TIMEOUT;
    if tokio::time::timeout_at(deadline, &mut renderer)
        .await
        .is_err()
    {
        renderer.abort();
        let _ = renderer.await;
    }
    console.finish(&console_report(&result), deadline).await;
    Ok(result)
}

fn console_report(result: &Session) -> String {
    let mut report = format!(
        "\nStatus: {}\nOutput: {}",
        result.status,
        crate::paths::display_path(&result.project.output)
    );
    if let Some(e) = &result.last_error {
        report.push('\n');
        report.push_str(&crate::paths::display_text(e));
    }
    report.push('\n');
    report
}

#[cfg(test)]
mod path_display_tests {
    use super::*;
    use crate::config::Project;

    #[test]
    fn completion_and_console_reports_format_paths_without_changing_task_or_answer() {
        let output = r"\\?\C:\프로젝트\결과.md";
        let error = r"file_access_error: \\?\UNC\server\share\원본.md";
        let mut session = Session::new(
            Project {
                output: output.into(),
                ..Default::default()
            },
            Config::compact_test(),
        );
        session.last_error = Some(error.into());
        session.completion_gaps = vec![error.into()];
        let report = gap_report(&session, None, None);
        assert!(report.starts_with("`C:\\프로젝트\\결과.md` 문서 작성을 마쳤습니다."));
        assert!(report.contains(r"file_access_error: \\server\share\원본.md"));
        let console = console_report(&session);
        assert!(console.contains("Output: C:\\프로젝트\\결과.md"));
        assert!(console.contains(r"file_access_error: \\server\share\원본.md"));
        let original_answer = r"소스 코드 원문: `\\?\C:\프로젝트\결과.md`";
        assert!(gap_report(&session, Some(original_answer), None).starts_with(original_answer));
        assert_eq!(session.project.output.to_str(), Some(output));
        assert_eq!(session.last_error.as_deref(), Some(error));
        assert_eq!(session.completion_gaps, vec![error]);
    }
}

fn consume_commands(s: &mut Session, commands: &mut mpsc::Receiver<RunCommand>) {
    while let Ok(command) = commands.try_recv() {
        match command {
            RunCommand::Configure(config) => s.pending_config = Some(*config),
            RunCommand::Tools(names) => {
                // Revalidate against the latest workflow, and supersede an
                // older model selection waiting for the next batch.
                s.active_tools = ToolRegistry::normalize_tool_selection(s, &names);
                s.pending_tools = None;
            }
        }
    }
}

fn apply_pending_config(s: &mut Session, prune_history: bool) -> Result<bool> {
    let Some(config) = s.pending_config.clone() else {
        return Ok(false);
    };
    // Validate on a private candidate. A rejected setting must not leave an
    // irreversible history prune behind. Follow-up questions cannot prune the
    // suspended task's history even if cleanup would make the settings fit.
    let mut candidate = s.clone();
    if prune_history {
        candidate.history.prune(config.history_bytes)?;
    }
    apply_config(&mut candidate, config)?;
    candidate.pending_config = None;
    *s = candidate;
    Ok(true)
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
    if s.config.model != copy.config.model {
        // Calibration samples describe the previous model's tokenizer.
        s.token_ratios.clear();
    }
    s.config = copy.config;
    Ok(())
}

#[cfg(test)]
mod review_gap_tests {
    use super::*;
    use crate::config::{Project, Secret};

    #[test]
    fn unread_citations_block_readiness_until_their_ranges_are_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                ..Config::compact_test()
            },
        );
        s.add_user("Document a.rs.".into());
        s.select_workflow("source_document").unwrap();
        let written = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create","text":"# Manual\n## A\na.rs:1\n## B\na.rs:2\n"}),
        )
        .unwrap();
        // The save itself names what was cited without being read.
        // Adjacent cited lines of one file merge into one unread range.
        assert_eq!(
            written["citation_check"]["unread_citation_count"], 1,
            "{written}"
        );
        // Two live runs wrote a whole manual and never registered an item:
        // readiness now depends only on the saved document and its reads.
        assert!(!ready_for_final(&mut s));
        let issues = s.run_guidance["document_readiness"]["issues"].clone();
        assert!(
            issues
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unread_citation"
                    && issue["path"] == "a.rs"
                    && issue["start_line"] == 1
                    && issue["end_line"] == 2),
            "{issues}"
        );
        // Closing mode asks for the same reads instead of bookkeeping.
        s.progress_recovery.closing = Some(Default::default());
        let instruction = closing_instruction(&s);
        assert!(instruction.contains("unread"), "{instruction}");
        s.progress_recovery.closing = None;
        tools::execute(&mut s, "file_read", json!({"path":"a.rs"})).unwrap();
        assert!(ready_for_final(&mut s));
        assert!(s.run_guidance.get("document_readiness").is_none());
        // A changed source makes the read stale and blocks the ready instruction.
        std::fs::write(dir.path().join("a.rs"), "fn changed() {}\nfn b() {}\n").unwrap();
        assert!(!ready_for_final(&mut s));
        assert_eq!(s.run_guidance["document_readiness"]["structural_ok"], false);
    }

    #[test]
    fn completion_readiness_and_final_gaps_use_the_current_result() {
        use tools::completion_review::{self as review, Gate};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ui.js"), "function openChat() {}\n").unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("manual.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                source_document_review: false,
                ..Config::compact_test()
            },
        );
        s.add_user("ui 사용자 매뉴얼 만들어줘".into());
        s.select_workflow("source_document").unwrap();
        tools::execute(&mut s, "file_read", json!({"path":"ui.js"})).unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create",
            "text":"# Chat\nOpen the chat screen. ui.js:1\n"}),
        )
        .unwrap();
        assert_eq!(
            review::begin(&mut s, "매뉴얼을 저장했습니다.").unwrap(),
            Gate::Review
        );
        review::request(&mut s).unwrap();
        review::finish(
            &mut s,
            &json!({"checks":[{"id":"R0","status":"unverified",
            "reason":"The chat screen section is incomplete","evidence":["E2"],"next_action":"Complete the chat section"}]})
            .to_string(),
        )
        .unwrap();
        assert!(!ready_for_final(&mut s));
        assert!(
            collect_gaps(&mut s, &[])
                .iter()
                .any(|gap| gap.contains("The chat screen section is incomplete"))
        );

        let expected = s.last_document_write.as_ref().unwrap().1.clone();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"replace_text","expected_hash":expected,"old_text":"Open the chat screen.","text":"Open the chat screen from the main window."}),
        )
        .unwrap();
        assert!(
            ready_for_final(&mut s),
            "A changed result must unblock the final answer that starts a new review"
        );
        let gaps = collect_gaps(&mut s, &[]);
        assert!(
            !gaps
                .iter()
                .any(|gap| gap.contains("The chat screen section is incomplete"))
        );
        assert!(
            gaps.iter()
                .any(|gap| gap == "완료 조건 검증 — 현재 결과를 마감 전에 검토하지 못했습니다.")
        );

        assert_eq!(
            review::begin(&mut s, "매뉴얼을 저장했습니다.").unwrap(),
            Gate::Review
        );
        let request = review::request(&mut s).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let file = payload["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "current_file")
            .unwrap();
        review::finish(&mut s, &json!({"checks":[{"id":"R0","status":"met",
            "reason":"Saved manual covers the chat screen","evidence":[file["id"]],"next_action":""}]}).to_string()).unwrap();
        assert!(collect_gaps(&mut s, &[]).is_empty());
        assert!(ready_for_final(&mut s));
    }

    #[test]
    fn final_gaps_keep_a_rejection_when_only_task_records_changed() {
        // Live run 2026-10-07: after the fifth rejection the model only
        // rewrote task_state, and the closing gap said the result was never
        // reviewed instead of naming the unmet requirement.
        use tools::completion_review::{self as review, Gate};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ui.js"), "function openChat() {}\n").unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("manual.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                source_document_review: false,
                ..Config::compact_test()
            },
        );
        s.add_user("ui 사용자 매뉴얼 만들어줘".into());
        s.select_workflow("source_document").unwrap();
        tools::execute(&mut s, "file_read", json!({"path":"ui.js"})).unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create",
            "text":"# Chat\nOpen the chat screen. ui.js:1\n"}),
        )
        .unwrap();
        assert_eq!(
            review::begin(&mut s, "매뉴얼을 저장했습니다.").unwrap(),
            Gate::Review
        );
        review::request(&mut s).unwrap();
        review::finish(
            &mut s,
            &json!({"checks":[{"id":"R0","status":"unmet",
            "reason":"Only the chat screen is covered","evidence":["E2"],"next_action":"Document the remaining screens"}]})
            .to_string(),
        )
        .unwrap();
        // Only the task record changes; the saved manual stays as reviewed.
        s.task.unresolved = vec!["Other screens were not read".into()];
        let gaps = collect_gaps(&mut s, &[]);
        assert!(
            !gaps
                .iter()
                .any(|gap| gap == "완료 조건 검증 — 현재 결과를 마감 전에 검토하지 못했습니다."),
            "{gaps:?}"
        );
        assert!(
            gaps.iter()
                .any(|gap| gap.starts_with("완료 조건 검증 — 마지막 검토 뒤 산출물은 그대로")),
            "{gaps:?}"
        );
        assert!(
            gaps.iter()
                .any(|gap| gap
                    == "완료 조건 R0 (마지막 검토: 미충족) — Only the chat screen is covered"),
            "{gaps:?}"
        );
        assert!(
            gaps.iter()
                .any(|gap| gap == "미확인 사항 — Other screens were not read"),
            "{gaps:?}"
        );
    }

    #[test]
    fn closing_reports_only_findings_reviewed_for_the_current_document() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("source.rs"),
            "fn run() { for _ in 0..5 {} }\n",
        )
        .unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                completion_review_enabled: false,
                ..Config::compact_test()
            },
        );
        s.add_user("Document the loop.".into());
        s.select_workflow("source_document").unwrap();
        tools::execute(&mut s, "file_read", json!({"path":"source.rs"})).unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create","text":"# Flow\nA while loop runs five times. source.rs:1\n"}),
        )
        .unwrap();
        tools::document_review::request(&mut s).unwrap();
        s.document_review.skipped_ranges.push((2, 2));
        tools::document_review::test_finish(
            &mut s,
            r#"{"issues":["Flow: the loop is for, not while"]}"#,
        )
        .unwrap();
        let gaps = collect_gaps(&mut s, &[]);
        assert!(gaps.iter().any(|gap| gap.contains("the loop is for")));
        assert!(
            gaps.iter()
                .any(|gap| gap == "문서 검토 — 2–2줄은 검토를 마치지 못했습니다.")
        );

        let expected = s.last_document_write.as_ref().unwrap().1.clone();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop","text":"A for loop"}),
        )
        .unwrap();
        let gaps = collect_gaps(&mut s, &[]);
        assert!(
            gaps.iter()
                .any(|gap| gap == "문서 검토 — 현재 문서를 마감 전에 검토하지 못했습니다.")
        );
        assert!(!gaps.iter().any(|gap| gap.contains("the loop is for")));
        assert!(!gaps.iter().any(|gap| gap.contains("2–2줄")));

        s.config.source_document_review = false;
        let gaps = collect_gaps(&mut s, &[]);
        assert!(!gaps.iter().any(|gap| gap.starts_with("문서 검토")));

        s.config.source_document_review = true;
        tools::document_review::request(&mut s).unwrap();
        tools::document_review::test_finish(&mut s, r#"{"issues":[]}"#).unwrap();
        let gaps = collect_gaps(&mut s, &[]);
        assert!(!gaps.iter().any(|gap| gap.starts_with("문서 검토")));
    }

    #[tokio::test]
    #[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
    async fn focused_live_review_does_not_report_previous_findings_after_an_edit() {
        assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
        let config_path = std::path::PathBuf::from(
            std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()),
        );
        let mut config = Config::load(&config_path, &std::collections::BTreeMap::new()).unwrap();
        if config_path.with_extension("credentials.json").exists() {
            let keys: std::collections::BTreeMap<String, String> = serde_json::from_slice(
                &std::fs::read(config_path.with_extension("credentials.json")).unwrap(),
            )
            .unwrap();
            config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
        }
        config.model = "stealth/space-bunny-alpha".into();
        config.source_document_review = true;
        config.completion_review_enabled = false;
        config.output_tokens = 2048;
        config.request_timeout_secs = 90;
        config.retries = 0;
        let mut project = config
            .projects
            .iter()
            .find(|project| project.name == "MnemoArc")
            .expect("registered MnemoArc project")
            .clone();
        let dir = tempfile::tempdir().unwrap();
        project.root = dir.path().into();
        project.output = dir.path().join("focused-live-manual.md");
        std::fs::write(
            dir.path().join("source.rs"),
            "fn run() {\n    for _ in 0..5 {\n        work();\n    }\n}\n",
        )
        .unwrap();
        let mut s = Session::new(project, config);
        s.select_workflow("source_document").unwrap();
        s.add_user(
            "source.rs의 run 함수에서 반복문 종류와 work 호출 횟수를 정확히 설명해줘.".into(),
        );
        tools::execute(&mut s, "file_read", json!({"path":"source.rs"})).unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create","text":"# 실행 흐름\nrun은 while 반복문으로 work를 열 번 실행합니다. source.rs:1-5\n"}),
        )
        .unwrap();
        let request = tools::document_review::request(&mut s).unwrap();
        let request_tokens = context::count(&request, &s.config.model);
        assert!(
            request_tokens <= 6_000,
            "review input grew to {request_tokens} tokens"
        );
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["more_document_pages"], false);
        assert_eq!(payload["more_evidence_pages"], false);
        let (tx, mut rx) = mpsc::channel(32);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let started = Instant::now();
        let completion = tokio::time::timeout(
            Duration::from_secs(120),
            OpenAiClient.complete(request, &s.config, CancellationToken::new(), tx),
        )
        .await
        .expect("focused review timeout")
        .expect("focused review request");
        drain.await.unwrap();
        let review_text = completion.text.clone();
        eprintln!(
            "[live] focused review: seconds={:.1} input_bound={} output_bound={} usage={:?} text={review_text}",
            started.elapsed().as_secs_f64(),
            request_tokens,
            s.config.output_tokens,
            completion.usage
        );
        tools::document_review::finish(&mut s, &review_text).unwrap();
        while s.document_review.pending {
            let request = tools::document_review::request(&mut s).unwrap();
            let (tx, mut rx) = mpsc::channel(32);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let remaining = Duration::from_secs(120)
                .checked_sub(started.elapsed())
                .expect("focused review deadline");
            let response = tokio::time::timeout(
                remaining,
                OpenAiClient.complete(request, &s.config, CancellationToken::new(), tx),
            )
            .await
            .expect("focused review validation timeout")
            .unwrap();
            drain.await.unwrap();
            tools::document_review::finish(&mut s, &response.text).unwrap();
        }
        assert!(
            tools::document_review::rejected_on_current_result(&s),
            "the live reviewer did not reject the deliberately false loop claim: {review_text}"
        );
        let previous_issues = s.document_review.issues.clone();
        let review_target_hash = tools::document_review::review_target_hash(&s)
            .unwrap()
            .to_owned();
        let expected = s.last_document_write.as_ref().unwrap().1.clone();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"replace_text","expected_hash":expected,"old_text":"while 반복문으로 work를 열 번","text":"for 반복문으로 work를 다섯 번"}),
        )
        .unwrap();
        let final_document_hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
        let final_message = force_finish(&mut s, "focused_live_test").expect("saved document");
        let expected_gap = "문서 검토 — 현재 문서를 마감 전에 검토하지 못했습니다.";
        let report = json!({
            "model":s.config.model,
            "project":s.project.name,
            "source_document_review_enabled":s.config.source_document_review,
            "completion_review_enabled":s.config.completion_review_enabled,
            "request_tokens_upper_bound":request_tokens,
            "output_tokens_upper_bound":s.config.output_tokens,
            "review_usage":completion.usage,
            "review_seconds":started.elapsed().as_secs_f64(),
            "review_text":review_text,
            "review_issues":previous_issues,
            "review_target_hash":review_target_hash,
            "final_document_hash":final_document_hash,
            "status":s.status,
            "completion_gaps":s.completion_gaps,
            "document_review_attempts":s.document_review.attempts,
            "completion_review_attempts":s.completion_review.attempts,
        });
        if let Ok(path) = std::env::var("MNEMOARC_DOC_REPORT") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        eprintln!("[live] focused close: {}", report);
        assert_ne!(final_document_hash, review_target_hash);
        assert_eq!(s.status, "complete_with_gaps");
        assert_eq!(s.document_review.attempts, 1);
        assert_eq!(s.completion_review.attempts, 0);
        assert_eq!(s.completion_gaps, [expected_gap]);
        assert!(final_message.contains(expected_gap));
        assert!(
            previous_issues
                .iter()
                .all(|issue| { !s.completion_gaps.iter().any(|gap| gap.contains(issue)) })
        );
    }
}

#[cfg(test)]
mod worker_wait_tests {
    use super::*;
    use crate::config::Project;

    #[tokio::test]
    async fn parallel_reads_share_cursor_positions_in_results_and_archives() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pages.md"),
            "정확한 커서 위치😀 and source evidence\n".repeat(200),
        )
        .unwrap();
        let mut s = session();
        s.project.root = dir.path().canonicalize().unwrap();
        s.config.model = "gpt-4o".into();
        s.config.result_tokens = 700;
        s.config.read_parallelism = 2;
        let calls: Vec<_> = (0..2)
            .map(|index| ToolCall {
                id: format!("parallel-position-{index}"),
                name: "file_read".into(),
                arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
            })
            .collect();
        let results = read_parallel(
            &mut s,
            &calls,
            &CancellationToken::new(),
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await;
        for result in results {
            assert_eq!(result["status"], "ok");
            let id = result["next_cursor"]["cursor"].as_str().unwrap();
            assert_eq!(
                s.file_cursors[id].offset,
                result["data"]["next_offset"].as_u64().unwrap() as usize
            );
            let archived = &s
                .history
                .read(result["archive_id"].as_u64().unwrap())
                .unwrap()
                .messages[0]["result"];
            let archived_id = archived["next_cursor"]["cursor"].as_str().unwrap();
            assert_eq!(
                s.file_cursors[archived_id].offset,
                archived["data"]["next_offset"].as_u64().unwrap() as usize
            );
            let continuation = tools::execute(&mut s, "file_read", json!({"cursor":id})).unwrap();
            assert_eq!(continuation["read_offset"], result["data"]["next_offset"]);
        }
    }

    #[tokio::test]
    async fn parallel_reads_preserve_suppression_and_owner_history() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("read.rs"),
            "fn observed() {}\n// nearby context\n",
        )
        .unwrap();
        let mut s = session();
        s.project.root = dir.path().canonicalize().unwrap();
        s.config.model = "gpt-4o".into();
        s.active_tools = ToolRegistry::optional_names();
        let read = tools::execute(
            &mut s,
            "file_read",
            json!({"path":"read.rs","start_line":1,"max_lines":1}),
        )
        .unwrap();
        for _ in 0..s.config.repeated_read_limit {
            s.history.push(
                vec![
                    json!({"role":"tool","content":json!({"status":"ok","data":read}).to_string()}),
                ],
                true,
            );
        }
        let history_bytes = s.history.bytes();
        let history_next_id = s.history.next_id;
        let calls: Vec<_> = [
            (
                "file_read",
                json!({"path":"read.rs","start_line":1,"max_lines":1}),
            ),
            (
                "file_read",
                json!({"path":"read.rs","start_line":1,"max_lines":1,"force_read":true}),
            ),
            (
                "source_search",
                json!({"path":"read.rs","query":"observed"}),
            ),
            ("file_list", json!({"mode":"paths"})),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, args))| ToolCall {
            id: format!("bounded-read-{index}"),
            name: name.into(),
            arguments: args.to_string(),
        })
        .collect();
        let results = read_parallel(
            &mut s,
            &calls,
            &CancellationToken::new(),
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await;
        assert!(results.iter().all(|result| result["status"] == "ok"));
        assert_eq!(results[0]["data"]["suppressed"], true);
        assert_eq!(results[1]["data"]["content"]["text"], "fn observed() {}");
        assert_eq!(s.history.bytes(), history_bytes);
        assert_eq!(s.history.next_id, history_next_id);

        // Retired observations and hashes from a previous file version must
        // not suppress a fresh read when rebuilding the worker's metadata.
        for bundle in s.history.bundles.iter_mut() {
            bundle.active = false;
        }
        let results = read_parallel(
            &mut s,
            &calls[..1],
            &CancellationToken::new(),
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await;
        assert_eq!(results[0]["data"]["content"]["text"], "fn observed() {}");
        for bundle in s.history.bundles.iter_mut() {
            bundle.active = true;
        }
        std::fs::write(dir.path().join("read.rs"), "fn changed() {}\n").unwrap();
        let results = read_parallel(
            &mut s,
            &calls[..1],
            &CancellationToken::new(),
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await;
        assert_eq!(results[0]["data"]["content"]["text"], "fn changed() {}");
        assert_eq!(s.history.bytes(), history_bytes);
        assert_eq!(s.history.next_id, history_next_id);
    }

    #[tokio::test]
    async fn dropping_a_tool_wait_cancels_the_outliving_read() {
        let (_sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        let child = cancel.child_token();
        let outcome = tokio::time::timeout(
            Duration::from_millis(10),
            await_tool_worker(
                session(),
                receiver,
                &cancel,
                child.clone(),
                ToolDeadline::new(
                    Duration::from_secs(60),
                    tokio::time::Instant::now() + Duration::from_secs(60),
                ),
                Duration::from_secs(1),
                false,
            ),
        )
        .await;
        assert!(outcome.is_err());
        assert!(child.is_cancelled());
        assert!(!cancel.is_cancelled());
    }
    fn session() -> Session {
        Session::new(Project::default(), Config::compact_test())
    }

    #[tokio::test]
    async fn reported_uncertain_writes_quarantine_following_calls_and_runs() {
        for cause in [
            "database_commit_uncertain: connection lost during commit",
            "database_rollback_uncertain: connection lost during rollback",
            "file_patch_rollback_failed: one changed file could not be restored",
            "tool_worker_panic: interrupted after a file write",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut s = session();
            s.project.root = dir.path().into();
            let shared = s.write_outcome_uncertain.clone();
            let outcome = tools::envelope(Err(anyhow::anyhow!(cause)));
            let (s, result) = received_tool_outcome(s.clone(), Ok((s, outcome)), true);
            assert_eq!(result["error"], cause);
            assert!(shared.load(Ordering::Acquire), "{cause}");
            let call = ToolCall {
                id: "following-write".into(),
                name: "file_write".into(),
                arguments: json!({"path":"must-not-exist.txt","content":"follow-up mutation"})
                    .to_string(),
            };
            let cancel = CancellationToken::new();
            let (s, result) = execute_one(
                s,
                call,
                &cancel,
                tokio::time::Instant::now() + Duration::from_secs(60),
            )
            .await;
            assert_eq!(result["status"], "error");
            assert!(!dir.path().join("must-not-exist.txt").exists());
            let (events, _receiver) = mpsc::channel(1);
            let blocked = run_session(s, Arc::new(OpenAiClient), cancel, events).await;
            assert_eq!(blocked.status, "blocked");
        }
        // A confirmed rollback or a read failure has no unresolved write.
        for (cause, external_write) in [
            (
                "file_patch_write_failed: completed file changes were rolled back",
                true,
            ),
            ("tool_worker_panic: read interrupted", false),
        ] {
            let s = session();
            let (s, _) = received_tool_outcome(
                s.clone(),
                Ok((s, tools::envelope(Err(anyhow::anyhow!(cause))))),
                external_write,
            );
            assert!(!s.write_outcome_uncertain.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn run_deadline_cancels_a_read_before_its_tool_timeout() {
        let (_sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        let child = cancel.child_token();
        let deadline = ToolDeadline::new(
            Duration::from_secs(5),
            tokio::time::Instant::now() + Duration::from_millis(10),
        );
        let (_, result) = tokio::time::timeout(
            Duration::from_millis(300),
            await_tool_worker(
                session(),
                receiver,
                &cancel,
                child.clone(),
                deadline,
                Duration::from_millis(20),
                false,
            ),
        )
        .await
        .expect("run deadline must bound a stuck read");
        assert_eq!(result["error"], "run_timeout");
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn expired_run_does_not_start_queued_reads_or_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
        let mut s = session();
        s.project.root = dir.path().into();
        s.config.read_parallelism = 1;
        let calls: Vec<_> = (0..4)
            .map(|i| ToolCall {
                id: format!("read-{i}"),
                name: "file_read".into(),
                arguments: json!({"path":"source.rs"}).to_string(),
            })
            .collect();
        let cancel = CancellationToken::new();
        let expired = tokio::time::Instant::now();
        let results = read_parallel(&mut s, &calls, &cancel, expired).await;
        assert_eq!(results.len(), 4);
        assert!(
            results
                .iter()
                .all(|result| result["error"] == "run_timeout")
        );
        assert!(s.sources.is_empty());
        let write = ToolCall {
            id: "write".into(),
            name: "file_write".into(),
            arguments: json!({"path":"late.rs","content":"late"}).to_string(),
        };
        let (_, result) = execute_one(s, write, &cancel, expired).await;
        assert_eq!(result["error"], "run_timeout");
        assert!(!dir.path().join("late.rs").exists());
    }

    #[tokio::test]
    async fn cancelled_read_does_not_wait_for_a_stuck_worker() {
        let backup = session();
        let (_sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (_, result) = tokio::time::timeout(
            Duration::from_millis(200),
            await_tool_worker(
                backup.clone(),
                receiver,
                &cancel,
                cancel.child_token(),
                ToolDeadline::new(
                    Duration::from_secs(5),
                    tokio::time::Instant::now() + Duration::from_secs(60),
                ),
                Duration::from_millis(20),
                false,
            ),
        )
        .await
        .expect("cancelled read must not join the worker");
        assert_eq!(result["error"], "cancelled");
        assert!(!backup.write_outcome_uncertain.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn unresolved_write_returns_and_quarantines_future_runs() {
        let backup = session();
        let (_sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        let (returned, result) = tokio::time::timeout(
            Duration::from_millis(200),
            await_tool_worker(
                backup.clone(),
                receiver,
                &cancel,
                cancel.child_token(),
                ToolDeadline::new(
                    Duration::from_millis(10),
                    tokio::time::Instant::now() + Duration::from_secs(60),
                ),
                Duration::from_millis(20),
                true,
            ),
        )
        .await
        .expect("external write wait must be bounded");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .starts_with("tool_worker_unresolved")
        );
        assert!(backup.write_outcome_uncertain.load(Ordering::Acquire));
        let (events, _receiver) = mpsc::channel(1);
        let blocked = run_session(returned, Arc::new(OpenAiClient), cancel, events).await;
        assert_eq!(blocked.status, "blocked");
        assert!(
            blocked
                .last_error
                .unwrap()
                .starts_with("tool_worker_unresolved")
        );
    }

    #[tokio::test]
    async fn write_finishing_during_grace_period_keeps_its_result() {
        let backup = session();
        let (sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let finished = backup.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send((finished, json!({"status":"ok","data":{"committed":true}})));
        });
        let (_, result) = await_tool_worker(
            backup.clone(),
            receiver,
            &cancel,
            cancel.child_token(),
            ToolDeadline::new(
                Duration::from_secs(5),
                tokio::time::Instant::now() + Duration::from_secs(60),
            ),
            Duration::from_millis(100),
            true,
        )
        .await;
        assert_eq!(result["data"]["committed"], true);
        assert!(!backup.write_outcome_uncertain.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn stopped_writes_that_confirm_cancellation_keep_the_stop_reason() {
        for reason in ["tool_timeout", "run_timeout", "cancelled"] {
            for message in [
                "cancelled",
                "cancelled: completed file changes were rolled back",
            ] {
                let backup = session();
                let finished = backup.clone();
                let (sender, receiver) = oneshot::channel();
                let cancel = CancellationToken::new();
                if reason == "cancelled" {
                    cancel.cancel();
                }
                let child = cancel.child_token();
                let worker_cancel = child.clone();
                tokio::spawn(async move {
                    // A write waiting at the shared gate observes only its
                    // cancellation token, not why the agent stopped it.
                    worker_cancel.cancelled().await;
                    let _ = sender.send((finished, tools::envelope(Err(anyhow::anyhow!(message)))));
                });
                let (_, result) = await_tool_worker(
                    backup.clone(),
                    receiver,
                    &cancel,
                    child,
                    ToolDeadline {
                        at: tokio::time::Instant::now(),
                        reason,
                    },
                    Duration::from_secs(1),
                    true,
                )
                .await;
                assert_eq!(
                    result["status"],
                    if reason == "cancelled" {
                        "cancelled"
                    } else {
                        "error"
                    },
                    "{result}"
                );
                assert!(
                    result["error"].as_str().unwrap().starts_with(reason),
                    "{result}"
                );
                assert_eq!(result["recovery"]["code"], reason);
                assert_eq!(
                    result["recovery"]["action"] == "stop",
                    reason == "cancelled"
                );
                assert_eq!(cancel.is_cancelled(), reason == "cancelled");
                assert!(!backup.write_outcome_uncertain.load(Ordering::Acquire));
            }
        }
    }
}

#[cfg(test)]
mod provider_outage_tests {
    use super::*;
    use crate::config::Project;
    use crate::llm::{Completion, CompletionError, LlmClient, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fails the first `outages` requests as an unavailable provider.
    struct Outage {
        outages: usize,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmClient for Outage {
        async fn complete(
            &self,
            _: Value,
            _: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.outages {
                return Err(CompletionError::new(
                    anyhow::anyhow!(
                        "provider_stream_error: {}",
                        r#"{"code":502,"message":"Provider returned an empty response"}"#
                    ),
                    3,
                    true,
                )
                .into());
            }
            Ok(Completion {
                text: "Saved answer".into(),
                usage: Some(Usage {
                    input: 100,
                    output: 7,
                    cached: None,
                }),
                ..Default::default()
            })
        }
    }

    async fn run(outages: usize) -> (Session, usize, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                model_context: Some(128_000),
                source_document_review: false,
                completion_review_enabled: false,
                run_timeout_secs: 3600,
                ..Config::compact_test()
            },
        );
        s.add_user("First request".into());
        let client = Arc::new(Outage {
            outages,
            calls: AtomicUsize::new(0),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move {
            let mut notices = Vec::new();
            while let Some(event) = rx.recv().await {
                if let AgentEvent::Notice { text, .. } = event {
                    notices.push(text);
                }
            }
            notices
        });
        let s = run_session(s, client.clone(), CancellationToken::new(), tx).await;
        let notices = drain.await.unwrap();
        (s, client.calls.load(Ordering::SeqCst), notices)
    }

    #[tokio::test(start_paused = true)]
    async fn waits_out_a_provider_outage_and_resends_the_request() {
        let (s, calls, notices) = run(2).await;
        assert_eq!(s.status, "complete", "{:?}", s.last_error);
        assert_eq!(calls, 3);
        // Two unanswered requests are not rounds: only the answered one counts.
        assert_eq!(s.task_rounds, 1);
        assert_eq!(s.progress_recovery.rounds_since_best, 0);
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("retrying in 10s (1/4)")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("retrying in 30s (2/4)"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lasting_outage_stops_with_the_request_count() {
        let (s, calls, _) = run(usize::MAX).await;
        assert_eq!(calls, PROVIDER_OUTAGE_WAITS.len() + 1);
        let error = s.last_error.unwrap();
        assert!(error.starts_with("provider_stream_error:"), "{error}");
        assert!(
            error.ends_with("provider unavailable after 15 requests over 4 waits"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod review_usage_tests {
    use super::*;
    use crate::{
        config::Project,
        llm::{Completion, CompletionError, Usage},
    };
    use std::sync::{Mutex, atomic::AtomicUsize};

    enum Failure {
        Provider(usize),
        /// Every quick retry was refused with HTTP 429 before generation.
        Refused(usize),
        Fatal,
        Malformed,
        InvalidCompletion(bool),
    }

    struct Reviewer {
        failure: Failure,
        requests: AtomicUsize,
        reserved_outputs: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for Reviewer {
        async fn complete(
            &self,
            request: Value,
            config: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            let payload = request["messages"][1]["content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or_default();
            let document = payload["source_document_review"] == true;
            let acceptance = payload["completion_review"] == true;
            if !document && !acceptance {
                return Ok(Completion {
                    text: "Saved source document.".into(),
                    usage: Some(Usage {
                        input: 11,
                        output: 7,
                        cached: None,
                    }),
                    ..Default::default()
                });
            }
            let index = self.requests.fetch_add(1, Ordering::SeqCst);
            match self.failure {
                Failure::Provider(failures) if index < failures => {
                    return Err(CompletionError::new(
                        anyhow::anyhow!("provider_stream_error: test outage"),
                        3,
                        true,
                    )
                    .into());
                }
                Failure::Refused(failures) if index < failures => {
                    let refused = |attempt| crate::llm::AttemptDiagnostic {
                        attempt,
                        code: "http_429".into(),
                        reason: "http_429: rate-limited upstream".into(),
                        action: crate::llm::AttemptAction::Retry,
                    };
                    return Err(CompletionError::new(
                        anyhow::anyhow!("http_429: rate-limited upstream"),
                        3,
                        true,
                    )
                    .with_diagnostics((1..=3).map(refused).collect())
                    .into());
                }
                Failure::Fatal => anyhow::bail!("test_review_failure"),
                Failure::Malformed if index == 0 => {
                    self.reserved_outputs
                        .lock()
                        .unwrap()
                        .push(config.output_tokens * 2);
                    return Err(CompletionError::new(
                        anyhow::anyhow!("invalid_tool_arguments: test malformed response"),
                        2,
                        false,
                    )
                    .into());
                }
                Failure::InvalidCompletion(receipt) if index == 0 => {
                    if !receipt {
                        self.reserved_outputs
                            .lock()
                            .unwrap()
                            .push(config.output_tokens);
                    }
                    return Ok(Completion {
                        calls: (0..=crate::llm::MAX_TOOL_CALLS)
                            .map(|id| ToolCall {
                                id: format!("call-{id}"),
                                name: "file_read".into(),
                                arguments: "{}".into(),
                            })
                            .collect(),
                        usage: receipt.then_some(Usage {
                            input: 17,
                            output: 23,
                            cached: None,
                        }),
                        ..Default::default()
                    });
                }
                _ => {}
            }
            let text = if document {
                json!({"issues":[]}).to_string()
            } else {
                let evidence = payload["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["id"] != "answer")
                    .map(|e| e["id"].clone())
                    .unwrap();
                json!({"checks":payload["criteria"].as_array().unwrap().iter().map(|criterion| {
                    json!({"id":criterion["id"],"status":"met","reason":"Fixture criterion satisfied",
                        "evidence":[evidence],"next_action":""})
                }).collect::<Vec<_>>()} ).to_string()
            };
            Ok(Completion {
                text,
                usage: Some(Usage {
                    input: 13,
                    output: 5,
                    cached: None,
                }),
                ..Default::default()
            })
        }
    }

    async fn run(completion: bool, failure: Failure) -> (Session, Arc<Reviewer>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn run() {}\n").unwrap();
        let mut session = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                model: "gpt-4o".into(),
                model_context: Some(128_000),
                source_document_review: !completion,
                completion_review_enabled: completion,
                run_timeout_secs: 3600,
                ..Config::compact_test()
            },
        );
        session.select_workflow("source_document").unwrap();
        session.add_user("Write a source document".into());
        tools::execute(&mut session, "file_read", json!({"path":"main.rs"})).unwrap();
        tools::execute(
            &mut session,
            "document_edit",
            json!({"action":"create","text":"# Flow\nCode. main.rs:1\n"}),
        )
        .unwrap();
        if completion {
            assert_eq!(
                tools::completion_review::begin_final(
                    &mut session,
                    "Saved source document.",
                    false
                )
                .unwrap(),
                tools::completion_review::Gate::Review
            );
        } else {
            tools::document_review::request(&mut session).unwrap();
            session.document_review.pending = true;
        }
        let reviewer = Arc::new(Reviewer {
            failure,
            requests: AtomicUsize::new(0),
            reserved_outputs: Mutex::new(Vec::new()),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(session, reviewer.clone(), CancellationToken::new(), tx).await;
        drain.await.unwrap();
        (result, reviewer)
    }

    fn review_usage(session: &Session) -> (usize, usize) {
        let usage = if session.config.source_document_review {
            (
                session.document_review.input_tokens,
                session.document_review.output_tokens,
            )
        } else {
            (
                session.completion_review.input_tokens,
                session.completion_review.output_tokens,
            )
        };
        // Only document review needs a separate, charged final answer request.
        let final_usage = if session.status == "complete" && session.config.source_document_review {
            (11, 7)
        } else {
            (0, 0)
        };
        assert_eq!(
            usage,
            (
                session.input_tokens - final_usage.0,
                session.output_tokens - final_usage.1
            )
        );
        usage
    }

    #[tokio::test]
    async fn failed_review_requests_are_attributed_before_stopping() {
        for completion in [false, true] {
            let (session, _) = run(completion, Failure::Fatal).await;
            assert_eq!(session.status, "blocked");
            assert!(review_usage(&session).0 > 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review_outage_retries_and_success_are_attributed_once() {
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::Provider(2)).await;
            assert_eq!(session.status, "complete", "{:?}", session.last_error);
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 3);
            let (input, output) = review_usage(&session);
            assert!(input > 13);
            assert_eq!(output, 5);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refused_attempts_are_not_charged_as_input() {
        // Live run 2026-10-06: four 429 sequences added ~0.5M unbilled input
        // to the run budget. A refusal with an HTTP status generated nothing.
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::Refused(2)).await;
            assert_eq!(session.status, "complete", "{:?}", session.last_error);
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 3);
            assert_eq!(review_usage(&session), (13, 5));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_review_outages_include_every_failed_attempt() {
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::Provider(usize::MAX)).await;
            assert_eq!(session.status, "blocked");
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 5);
            assert!(review_usage(&session).0 > 0);
        }
    }

    #[tokio::test]
    async fn malformed_review_recovery_attributes_reserved_output() {
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::Malformed).await;
            assert_eq!(session.status, "complete", "{:?}", session.last_error);
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 2);
            assert_eq!(
                review_usage(&session).1,
                reviewer.reserved_outputs.lock().unwrap()[0] + 5
            );
        }
    }

    #[tokio::test]
    async fn invalid_review_completion_keeps_its_usage_receipt() {
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::InvalidCompletion(true)).await;
            assert_eq!(session.status, "complete", "{:?}", session.last_error);
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 2);
            assert_eq!(review_usage(&session), (30, 28));
        }
    }

    #[tokio::test]
    async fn invalid_review_completion_without_usage_attributes_reserved_output() {
        for completion in [false, true] {
            let (session, reviewer) = run(completion, Failure::InvalidCompletion(false)).await;
            assert_eq!(session.status, "complete", "{:?}", session.last_error);
            assert_eq!(reviewer.requests.load(Ordering::SeqCst), 2);
            assert_eq!(
                review_usage(&session).1,
                reviewer.reserved_outputs.lock().unwrap()[0] + 5
            );
        }
    }
}
