use crate::{
    config::Config,
    console,
    context::{self, ContextManager},
    llm::{LlmClient, OpenAiClient, ToolCall},
    session::{Session, TodoWatch},
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
const LENGTH_RECOVERY_LIMIT: usize = 8;
/// Model requests available once closing mode starts. The runtime finishes
/// the document itself when they are spent.
pub const CLOSING_ROUND_LIMIT: usize = 12;
/// Requests at this run's average duration that the closing time reserve
/// keeps: the closing write, the final answer and one repair of a rejected
/// final. Should the deadline fall before a second final, the runtime
/// reports the same unresolved items that final would have been accepted
/// with.
const CLOSING_REQUEST_RESERVE: f64 = 3.0;
const MAX_REPORTED_GAPS: usize = 30;

/// Requests without a new best progress score before closing mode. Earlier
/// stages (1x focus, 2x narrowed tools) use the same counter.
fn closing_stall_limit(config: &Config) -> usize {
    config.stall_round_limit.saturating_mul(3)
}

/// One monotonic measure of document progress: the best document shape and
/// the project source lines delivered so far. Plan bookkeeping is excluded;
/// completing and reopening the same to-do is not progress.
fn progress_score(s: &Session) -> usize {
    s.progress_recovery.best_document_section_count * 2
        + s.progress_recovery.best_document_content_lines
        + s.progress_recovery.evidence_credit
}

/// Whether the output's open issues (document_audit's issue count plus
/// flagged citation mismatches) fell
/// below the fewest seen in this run. The repairs the final answer requires
/// (citation paths and ranges, format, citations narrowed to read lines)
/// add no section, line or evidence: a live run fixed 25 citation paths over
/// 24 requests and was closed as stalled. The fewest count only falls, so
/// breaking and repairing the same text is credited once.
fn document_issues_fell(s: &mut Session) -> bool {
    if !s.is_document_work() {
        return false;
    }
    let known = s.progress_recovery.document_issues_hash.clone();
    let Some((digest, count)) = tools::document_issue_count(s, known.as_deref()) else {
        return false;
    };
    let recovery = &mut s.progress_recovery;
    recovery.document_issues_hash = Some(digest);
    let fell = recovery
        .fewest_document_issues
        .is_some_and(|fewest| count < fewest);
    if recovery
        .fewest_document_issues
        .is_none_or(|fewest| count < fewest)
    {
        recovery.fewest_document_issues = Some(count);
    }
    fell
}

/// Cited ranges of the output document never delivered as complete lines of
/// the current file version; zero before the document exists. An existing
/// output this task has not written yet counts too: correction runs on an
/// existing document were told of no unread citation and learned of 24 to
/// 51 only when the final answer was refused.
fn unread_citation_count(s: &Session) -> usize {
    if !s.is_document_work() {
        return 0;
    }
    tools::unread_citations(s).map_or(0, |unread| unread.len())
}

/// Document edits listed after a checkpoint (see describe_document_edit).
const CHECKPOINT_EDITS_LISTED: usize = 6;

/// A short account of a document edit, for the model once a checkpoint has
/// cleared the edit's result: a live model resent a citation fix it had
/// saved during cleanup with the old hash, then applied it again inside the
/// fixed text ("src/src/agent.rs").
fn describe_document_edit(name: &str, arguments: &str) -> String {
    const SHOWN: usize = 4;
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let operations = if name == "document_edit_batch" {
        args["edits"].as_array().cloned().unwrap_or_default()
    } else {
        vec![args]
    };
    let clip = |text: &str| {
        let clipped: String = text.chars().take(60).collect();
        if clipped.len() < text.len() {
            format!("{clipped}…")
        } else {
            clipped
        }
    };
    let mut parts: Vec<String> = operations
        .iter()
        .take(SHOWN)
        .map(|operation| {
            let action = operation["action"].as_str().unwrap_or("edit");
            match (
                operation["old_text"].as_str(),
                operation["text"].as_str(),
                operation["section"].as_str(),
            ) {
                (Some(old), Some(new), _) => {
                    format!("{action} {:?} -> {:?}", clip(old), clip(new))
                }
                (_, _, Some(section)) => {
                    format!(
                        "{action} {:?}",
                        clip(section.lines().last().unwrap_or(section))
                    )
                }
                (_, Some(text), _) => format!(
                    "{action} beginning {:?}",
                    clip(
                        text.lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("")
                    )
                ),
                _ => action.to_owned(),
            }
        })
        .collect();
    if operations.len() > SHOWN {
        parts.push(format!("and {} more", operations.len() - SHOWN));
    }
    format!("{name}: {}", parts.join("; "))
}

/// Keep a record of a reply that carried no tool call. A reply cut at the
/// output limit or made during a checkpoint is settled at once; for the
/// others it returns true and settle_reply records the outcome once the
/// runtime has handled them.
fn note_reply(s: &mut Session, completion: &crate::llm::Completion) -> bool {
    if s.replies_without_calls.len() >= crate::session::REPLY_RECORDS {
        s.replies_without_calls.pop_front();
    }
    let (finish, outcome) = if completion.length_limited {
        ("length", "length_recovery")
    } else if s.checkpoint.is_some() {
        ("stop", "checkpoint")
    } else {
        ("stop", "pending")
    };
    s.replies_without_calls
        .push_back(crate::session::ReplyWithoutCalls {
            round: s.task_rounds,
            finish: finish.into(),
            output_tokens: completion.usage.as_ref().map(|usage| usage.output),
            reasoning_tokens: completion.reasoning_tokens,
            reasoning_chars: completion.reasoning_chars,
            text_chars: completion.text.chars().count(),
            discarded_tool_calls: completion.discarded_tool_calls,
            outcome: outcome.into(),
        });
    outcome == "pending"
}

/// Record what the runtime did with the last reply without calls: accepted
/// it as the final answer, or sent the run back to work with the error named
/// by its code. Every refusal sets that error, so the same refusal twice in
/// a row names it twice; a live run showed 25 repeated refusals as
/// "continued" when only changes were compared.
fn settle_reply(s: &mut Session) {
    let outcome = if matches!(s.status.as_str(), "complete" | "complete_with_gaps") {
        format!("accepted ({})", s.status)
    } else {
        s.last_error.as_deref().map_or_else(
            || "continued".to_owned(),
            |error| {
                let code = error.split(':').next().unwrap_or(error);
                if !code.is_empty() && !code.contains(' ') {
                    code.to_owned()
                } else {
                    error.chars().take(80).collect()
                }
            },
        )
    };
    if let Some(reply) = s
        .replies_without_calls
        .back_mut()
        .filter(|reply| reply.outcome == "pending")
    {
        reply.outcome = outcome;
    }
}

/// Whether the configured output exists on disk, written before this task
/// or by it.
fn output_exists(s: &Session) -> bool {
    tools::output_path(&s.project).is_ok_and(|path| path.is_file())
}

/// The task phase a model declared, as it steers this request. A source
/// document has no answer stage: a model that declared one (8 of 120 live
/// document runs) got the chat instruction "Answer the user now ... do not
/// save memory before answering a simple explanation" on every later
/// request, so it is read as verification, the stage before the final
/// answer.
fn declared_phase(phase: &str, document_work: bool) -> &str {
    if document_work && phase == "answer" {
        "verify"
    } else {
        phase
    }
}

/// run_guidance.instruction for a phase when no recovery or readiness
/// instruction replaces it. budget_phase tells a draft the budget reached
/// from one kept from earlier in the task: the task phase persists across
/// runs, so a resumed run starts in draft without spending half its budget.
fn phase_instruction(
    phase: &str,
    budget_phase: &str,
    answer_workflow: bool,
    todo_open: bool,
) -> &'static str {
    match phase {
        // An open to-do refuses the final answer, so "answer now" would be
        // refused until it is done.
        "answer" if todo_open => {
            "Finish the open to-dos from the evidence already gathered, reading only a concrete missing fact, then answer."
        }
        "answer" => {
            "Answer the user now from gathered evidence; read further only for a concrete missing fact the question requires."
        }
        "verify" => {
            "Verify: read the evidence still missing and repair known issues; qualify a fact that stays unverified where its claim appears."
        }
        "draft" if budget_phase == "draft" => {
            "Half of the run budget is spent: write the sections whose evidence is in context, and read only what the unwritten sections still need."
        }
        "draft" => {
            "Draft phase: write the sections whose evidence is in context, and read only what the unwritten sections still need."
        }
        _ if answer_workflow => {
            "Investigate: locate and read what the question needs, then answer once the evidence is sufficient."
        }
        _ => {
            "Investigate: read what the next planned section needs. You judge from the request and the evidence when the document is complete."
        }
    }
}

fn closing_instruction(s: &Session) -> String {
    let closing = s.progress_recovery.closing.as_ref();
    let remaining = CLOSING_ROUND_LIMIT.saturating_sub(closing.map_or(0, |c| c.rounds));
    if !s.document_written && output_exists(s) {
        return format!(
            "Closing mode: this task has not changed the existing document yet and source reading is withheld. Save the corrections the request asks for NOW with document_edit or document_edit_batch (replace_text or section; the document exists, so not create), from the evidence already gathered. Then give a concise final answer. At most {remaining} requests remain."
        );
    }
    if !s.document_written {
        return format!(
            "Closing mode: no document is saved yet and source reading is withheld. Create the document NOW with document_edit action=create holding its first requested section, from the evidence already gathered; add each other requested section with its own save, then give a concise final answer. At most {remaining} requests remain; without a saved document the run stops unfinished."
        );
    }
    format!(
        "Closing mode: finish the requested document now from the evidence already gathered; discovery tools are withheld. 1) Write any missing requested section. 2) Resolve the document_readiness issues: read each unread cited range or narrow its citation, send the citation_path fixes together in one document_edit_batch, and correct a citation that has no ready fix to an existing project path:start-end or remove it. 3) Complete the to-dos whose work is done with actual results and remove only obsolete ones; leave unfinished ones open for the runtime to report, then give a concise final answer. At most {remaining} requests remain; afterwards the runtime ends the run with the saved document and reports what remains unresolved."
    )
}

/// Requests before an empty plan of document work is pointed out.
const PLAN_CHECK_START: usize = 3;

/// plan_check key of the one comparison of a new plan with the request.
const PLAN_STRUCTURE_KEY: &str = "structure";
/// Plan items listed in that comparison, and characters kept of each.
const PLAN_STRUCTURE_ITEMS: usize = 20;
const PLAN_STRUCTURE_ITEM_CHARS: usize = 120;

/// Asks once, as soon as a plan exists, whether it follows the request.
/// A live run planned five "understand <module>" items and one section per
/// module for a request about the procedure from question to completion,
/// then wrote a module tour that never reached the run loop or completion.
fn plan_structure_check(task: &crate::session::TaskState) -> String {
    let pending: Vec<_> = task.todos.iter().filter(|item| !item.done).collect();
    let mut items: Vec<String> = pending
        .iter()
        .take(PLAN_STRUCTURE_ITEMS)
        .map(|item| {
            let text: String = item.text.chars().take(PLAN_STRUCTURE_ITEM_CHARS).collect();
            format!("{} {text}", item.id)
        })
        .collect();
    if pending.len() > PLAN_STRUCTURE_ITEMS {
        items.push(format!("and {} more", pending.len() - PLAN_STRUCTURE_ITEMS));
    }
    format!(
        "Before reading further, check task_plan against latest_request as the Plan section says: for a flow or procedure, one item per step in the order it happens, not per module or file. If it does not match, restructure it now in one task_plan apply, then continue with the first item. Items: {}",
        items.join("; ")
    )
}

/// A state in which task_plan no longer matches the work, pointed out once in
/// run_guidance.plan_check. A fixed reminder every few requests would repeat
/// what run_guidance already shows and invite plan rewrites; a live run
/// instead kept working for 100 requests with an empty plan after its first
/// plan call was rejected.
fn plan_check(s: &mut Session) -> Option<String> {
    if !s.is_document_work() || s.checkpoint.is_some() || s.progress_recovery.closing.is_some() {
        return None;
    }
    let sections = s.progress_recovery.best_document_section_count;
    let rounds = s.task_rounds;
    let stall_limit = s.config.stall_round_limit;
    let current = s.task.current_todo().map(|item| item.id.clone());
    let document = tools::output_path(&s.project)
        .and_then(|path| tools::read_text(&path))
        .ok()
        .map(|doc| tools::hash(doc.as_bytes()));
    let watch = &mut s.progress_recovery.todo_watch;
    match (current, watch.as_ref()) {
        (Some(id), Some(watched)) if watched.id == id => {}
        (Some(id), _) => {
            *watch = Some(TodoWatch {
                id,
                since_round: rounds,
                sections,
                from_round: rounds,
                document,
            })
        }
        (None, _) => *watch = None,
    }
    let checkpoint_done = s.checkpoints_completed > s.progress_recovery.plan_checkpoints_seen;
    s.progress_recovery.plan_checkpoints_seen = s.checkpoints_completed;
    let (key, text) = if s.task.todos.is_empty() {
        if rounds < PLAN_CHECK_START {
            return None;
        }
        (
            format!("empty:{}", s.task.plan_revision),
            format!(
                "task_plan is empty: create it now with task_plan action=apply and expected_revision {}, one item per section of the request (for a flow or procedure, one per step in the order it happens), then work through it.",
                s.task.plan_revision
            ),
        )
    } else if !s
        .progress_recovery
        .plan_checks_shown
        .contains(PLAN_STRUCTURE_KEY)
    {
        (PLAN_STRUCTURE_KEY.to_owned(), plan_structure_check(&s.task))
    } else if checkpoint_done {
        // Before the first save there is no document to compare with; a live
        // run was told to check its plan "against the saved document" at five
        // checkpoints before it wrote anything.
        let against = if s.document_written {
            "the saved document"
        } else if output_exists(s) {
            "the document, which this task has not changed yet"
        } else {
            "the work done so far (no document is saved yet)"
        };
        (
            format!("checkpoint:{}", s.checkpoints_completed),
            format!(
                "A checkpoint just completed and task_plan was preserved. Check current_todo and the remaining items against {against}, and update them if the work has moved on, before continuing."
            ),
        )
    } else if let Some(watch) = &mut s.progress_recovery.todo_watch {
        let id = watch.id.clone();
        if sections > watch.sections {
            // Count the to-do's staleness from this section on. Counting it
            // from when the to-do became current kept this check true, and a
            // live run's to-do stayed open 25 requests without the stale check.
            watch.sections = sections;
            watch.from_round = rounds;
            (
                format!("section:{id}:{sections}"),
                format!(
                    "A section was saved while current_todo {id} stayed open. If its work is done, complete it with the observed result; otherwise update the plan so it matches the work."
                ),
            )
        } else if rounds.saturating_sub(watch.from_round) >= stall_limit {
            let requests = rounds.saturating_sub(watch.since_round);
            (
                format!("stale:{id}:{}", watch.from_round),
                format!(
                    "current_todo {id} has been current for {requests} requests. If the work has moved on, complete or update it so the plan matches what you are doing."
                ),
            )
        } else {
            return None;
        }
    } else {
        (format!("done:{}", s.task.plan_revision), "Every item in task_plan is done. If a requested section or area is still unwritten, add items for it; otherwise give the final answer once the document is complete.".to_owned())
    };
    // Every key of a kind grows (a count, a revision or a round), so an
    // earlier key of the same kind is never checked again. Kept, they added
    // one entry for each checkpoint and plan revision of a long task.
    let shown = &mut s.progress_recovery.plan_checks_shown;
    if shown.contains(&key) {
        return None;
    }
    let kind = key.split(':').next().unwrap_or_default();
    shown.retain(|old| old.split(':').next() != Some(kind));
    shown.insert(key);
    Some(text)
}

/// Everything a complete_with_gaps result must disclose, derived from state.
fn collect_gaps(s: &mut Session) -> Vec<String> {
    let mut gaps = Vec::new();
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
        if tools::citation_count(s) == 0 {
            gaps.push(
                "소스 인용 — 문서에 경로:줄 형식의 소스 인용이 없어 소스와 대조하지 못했습니다."
                    .into(),
            );
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

// A document that passes its audit can still lack requested sections; a live
// run read an older wording as permission to remove the to-dos for unwritten
// sections.
const PLAN_CLOSEOUT_INSTRUCTION: &str = "The saved document passes its audit, and to-dos are open: task.todos lists them, and plan_closeout.more_items any beyond that list. A to-do whose work is not done yet, such as a requested section still missing from the document, is NOT obsolete: do that work first. When several listed to-dos are done, close them in ONE task_plan apply using plan_closeout.expected_revision, one operation per item in the listed order.";

/// Added to a focused recovery or a draft/verify instruction when
/// plan_closeout is present.
const CLOSEOUT_NOTE: &str = "Open to-dos are in task.todos: close those whose work is done in one task_plan apply with plan_closeout.expected_revision.";

/// The final answer of a source document that cites no project source.
const DOCUMENT_CITATIONS_MISSING: &str = "document_citations_missing: the saved document cites no project source as path:start-end (for example backend/src/server.js:12-30), so it cannot be checked against the source and the final answer is refused. Cite the sources next to the claims they support, from ranges you have read (file_read a range first if needed), then give the final answer.";

/// Closing mode only: the runtime is ending the run and nothing is left to
/// repair. Outside closing the model judges when the document is complete.
const READY_FOR_FINAL_INSTRUCTION: &str = "Ready to finish: the document is saved, its audit passes and no to-do remains. If a requested section is still missing, write it first; otherwise give the concise final answer now. Do not inspect or audit again unless you change the document.";

/// Document work whose own bookkeeping is finished: the next useful step is
/// the final answer.
fn ready_for_final(s: &mut Session) -> bool {
    // Audit even while a to-do is open: closing mode reads the document's
    // problems from document_readiness, which a live closing never received
    // because its last to-do stayed open.
    let ready = ready_except_plan(s);
    ready && s.task.current_todo().is_none()
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
        && tools::verify_document_write(s).is_ok();
    if !bookkeeping_ready {
        return false;
    }
    // Every citation must be well-formed and read. Use the same audit as
    // final acceptance, including its freshness revalidation.
    match tools::audit_document(s) {
        // A document without citations has every cited range read trivially;
        // its final answer is refused, so it is not ready either.
        Ok(audit) if audit["structural_ok"] == true && tools::citation_count(s) == 0 => {
            s.run_guidance["document_readiness"] = json!({"structural_ok":true,
                "citations_checked":0,"error":"document_citations_missing"});
            false
        }
        // A passing audit is stated as a fact, so the model need not call
        // document_audit to confirm an unchanged document. It is not a
        // finish instruction: one let concise models stop after the first
        // sections.
        Ok(audit) if audit["structural_ok"] == true => {
            s.run_guidance["document_readiness"] = json!({"structural_ok":true,
                "citations_checked":audit["citations_checked"]});
            true
        }
        // issue_kinds (from the audit) still shows the whole set: a live
        // closing saw five of 17 path issues and never their scale.
        Ok(mut audit) => {
            if let Some(issues) = audit["issues"].as_array_mut() {
                issues.truncate(5);
            }
            audit["next_offset"] =
                json!((audit["issue_count"].as_u64().unwrap_or(0) > 5).then_some(5));
            // Repeated on every request until fixed: the issues, not the
            // audit's hash, revision (the runtime pages with it) and check
            // counters; format warnings stay when there are any.
            let warnings = audit["format_check"]["warnings"].clone();
            if let Some(fields) = audit.as_object_mut() {
                for internal in ["hash", "revision", "total_lines", "format_check"] {
                    fields.remove(internal);
                }
            }
            if warnings
                .as_array()
                .is_some_and(|warnings| !warnings.is_empty())
            {
                audit["format_warnings"] = warnings;
            }
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

/// Document headings shown with the coverage check.
const COVERAGE_OUTLINE_HEADINGS: usize = 60;

/// The error that sends the first final answer of a task back once to
/// compare the document with the request. A live run closed every planned
/// module section, passed its audit and finished with 43% of its budget
/// left, never covering the run loop or completion the request asked for.
fn coverage_check(s: &mut Session) -> Option<String> {
    if s.progress_recovery.coverage_checked {
        return None;
    }
    s.progress_recovery.coverage_checked = true;
    let (headings, total) = tools::output_outline(s, COVERAGE_OUTLINE_HEADINGS).ok()?;
    let more = if total > headings.len() {
        format!(" (and {} more headings)", total - headings.len())
    } else {
        String::new()
    };
    // Shown with the one coverage check, the last look before the final: a
    // live correction kept a claim whose cited lines hold other code. The
    // check only compares names, so a claim that reading confirms stays: a
    // live run told only to correct a right citation re-read it for 30
    // requests.
    let mismatches = tools::citation_mismatches(s)
        .map(|check| {
            let items: Vec<String> = check["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    format!(
                        "line {} {} names {}",
                        item["line"],
                        item["citations"],
                        item["names"]
                    )
                })
                .collect();
            format!(
                " Also, {} claim(s) name code that the lines they cite never mention: {}. Read those lines once: correct a claim they do not support, or cite the lines that hold that code; if they do support it (for example they lie inside the named function), leave it as it is. This check only compares names, so a correct claim may stay flagged: do not read the same lines again for it.",
                check["flagged"],
                items.join("; ")
            )
        })
        .unwrap_or_default();
    // A live document finished with unread functions shown as code in a
    // section that cited nothing.
    let uncited = tools::uncited_code(s)
        .map(|check| {
            let items: Vec<String> = check["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    format!(
                        "line {} {} ({} code at line {})",
                        item["line"],
                        item["heading"],
                        item["language"].as_str().unwrap_or(""),
                        item["code_line"]
                    )
                })
                .collect();
            format!(
                " Also, {} section(s) show code but cite no source lines: {}. Read where that code is defined, make the block match it and cite its path:start-end in the section, or say in the text that the block is only an example.",
                check["flagged"],
                items.join("; ")
            )
        })
        .unwrap_or_default();
    Some(format!(
        "request_coverage_check: before finishing, compare the saved document with latest_request. Its headings are: {}{more}. Check that a section explains each part the request asks for; for a flow or procedure, every step from its start to its end. If a part has no section, add a task_plan item for it and write that section from source you have read (read more first if needed). If every part is covered, give the final answer again.{mismatches}{uncited}",
        headings.join(" | ")
    ))
}

/// Unresolved items shown with one unresolved closeout.
const UNRESOLVED_CLOSEOUT_ITEMS: usize = 10;

/// The error that sends a final answer back once when task.unresolved has
/// items not shown before. A finished document reports every unresolved
/// item as unconfirmed; a live run listed its pending to-dos there,
/// finished them and still ended complete_with_gaps reporting all three.
fn unresolved_closeout(s: &mut Session) -> Option<String> {
    // Only the task's current items are kept: every item a long task ever
    // listed stayed here before.
    let unresolved = &s.task.unresolved;
    let shown = &mut s.progress_recovery.unresolved_shown;
    shown.retain(|item| unresolved.contains(item));
    if unresolved.iter().all(|item| shown.contains(item)) {
        return None;
    }
    s.progress_recovery
        .unresolved_shown
        .extend(s.task.unresolved.iter().cloned());
    let items: Vec<_> = s
        .task
        .unresolved
        .iter()
        .take(UNRESOLVED_CLOSEOUT_ITEMS)
        .map(|item| format!("{item:?}"))
        .collect();
    let more = s
        .task
        .unresolved
        .len()
        .saturating_sub(UNRESOLVED_CLOSEOUT_ITEMS);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "task_unresolved_open: task.unresolved has {} item(s) that the result will report as unconfirmed: {}{more}. Check each against the saved document and the work done: when it is resolved (its work is done or the document covers it), drop it with task_state action=update, patch.unresolved set to only the items still open. Keep an item only if it is genuinely still unconfirmed. Then give the final answer again.",
        s.task.unresolved.len(),
        items.join("; ")
    ))
}

/// The open to-dos in order, bounded by one task_plan batch.
fn plan_closeout(s: &Session) -> Value {
    let pending: Vec<_> = s.task.todos.iter().filter(|item| !item.done).collect();
    json!({
        "expected_revision":s.task.plan_revision,
        "pending_count":pending.len(),
        "items":pending.iter().take(16).map(|item| json!({"id":item.id,"text":item.text})).collect::<Vec<_>>()
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

/// last_error prefixes of a refused final answer: only these reach the model
/// as run_guidance.finalization_error, which the prompt describes as the
/// reason a final answer was not accepted. A batch-limit or empty-reply error
/// set later in the same repair stretch is not one. A refusal also stops
/// showing once progress resets the finalization attempts, so a citation
/// added after document_citations_missing is not refused again in state.
const FINAL_REFUSALS: [&str; 10] = [
    "task_plan_pending:",
    "task_unresolved_open:",
    "request_coverage_check:",
    "document_citations_missing:",
    "Document format/evidence audit",
    "Document audit failed",
    "The requested source document has not been saved",
    "document_write_verification_failed",
    "document_changed_after_write",
    "New settings are waiting",
];

fn final_refusal(error: &str) -> bool {
    FINAL_REFUSALS
        .iter()
        .any(|prefix| error.starts_with(prefix))
}

/// Why a final answer went back to work.
fn finalization_notice(error: Option<&str>) -> &'static str {
    match error {
        Some(error)
            if error.starts_with("Document format/evidence audit")
                || error.starts_with("Document audit failed") =>
        {
            "The document audit found problems; returning to fix them within the remaining budget"
        }
        Some(error) if error.starts_with("The requested source document has not been saved") => {
            "The requested document is not saved yet; returning to write it within the remaining budget"
        }
        Some(error) if error.starts_with("document_citations_missing:") => {
            "The document cites no source yet; returning to add source citations within the remaining budget"
        }
        _ => "Final checks failed; returning to the remaining work within the remaining budget",
    }
}

/// Consecutive empty replies retried before a non-document run stops.
const EMPTY_COMPLETION_RETRIES: usize = 2;

/// Consecutive empty replies after which document work enters closing.
/// Document recovery used to retry an identical request until the stall
/// ladder closed it: a live run spent 24 rounds (1.4M input tokens) there.
const EMPTY_DOCUMENT_CLOSING: usize = 3;

fn finish_cause_text(cause: &str) -> &'static str {
    match cause {
        "run_budget_exhausted" => "실행 예산이 소진되어 마감했습니다.",
        "run_timeout" => "실행 시간이 끝나 마감했습니다.",
        "closing_round_limit" => "마감 단계의 요청 한도에 도달해 마감했습니다.",
        "budget" => "마감 예산에 도달해 마감했습니다.",
        "stall" => "진행이 오래 멈춰 마감했습니다.",
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
    s.continuation = None;
    s.completion_gaps = collect_gaps(s);
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

/// What each stall counter reports in recovery_reason, from focus to
/// exhaustion.
const REPEATED_OUTCOME_REASON: &str = "Repeated actions produced no new result.";
const NAVIGATION_STALL_REASON: &str =
    "Navigation and bookkeeping alone have not advanced the result for many requests.";
const ARTIFACT_CHURN_REASON: &str = "Different rewrites have not completed a milestone; fix the current section's specific problems instead of another general rewrite.";

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
    "artifact_progress_exhausted: many distinct edits produced no completed task item or verified section; current files and requirements are retained",
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
    let budget = if s.checkpoint.is_some() {
        ContextManager::cleanup_result_budget(&s.config)
    } else {
        s.config.batch_budget()
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
        /// What the call read, searched or changed (tools::call_summary).
        summary: String,
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
    let timeout = s.config.tool_timeout_secs;
    let deadline = ToolDeadline::new(Duration::from_secs(timeout), run_deadline);
    let backup = s.clone();
    let child = cancel.child_token();
    let tool = call.name.clone();
    let external_write = tools::external_write(&call.name);
    let receiver = match spawn_tool_worker(s, call, child.clone()) {
        Ok(receiver) => receiver,
        Err(error) => return (backup, tools::envelope(Err(error.into()))),
    };
    let (session, result) = await_tool_worker(
        backup,
        receiver,
        cancel,
        child,
        deadline,
        Duration::from_secs(1),
        external_write,
    )
    .await;
    if result["error"] == "tool_timeout" {
        let error = anyhow::anyhow!(stop_message("tool_timeout", &tool, timeout));
        return (session, tools::envelope(Err(error)));
    }
    (session, result)
}

/// Why a tool call was stopped at its deadline. A search or listing stopped
/// at the tool time limit stops the same way again over the same scope, so
/// it names the scope as the cause: a bare tool_timeout read as transient
/// and invited the identical call.
fn stop_message(reason: &str, tool: &str, secs: u64) -> String {
    if reason == "tool_timeout" && matches!(tool, "file_list" | "source_search") {
        format!(
            "scope_timeout: {tool} did not finish within the {secs}s tool time limit, and the same call would stop again; narrow its scope (path, path_glob, or a more specific query or pattern) or split it into smaller calls"
        )
    } else {
        reason.to_owned()
    }
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
/// A read that may run on a parallel worker. The worker's temporary session
/// holds no closing or progress-recovery state, so a read those withhold
/// goes through execute_one, which refuses it: run in parallel, a withheld
/// file_list or source_search ran in closing mode.
fn parallel_read(s: &Session, name: &str) -> bool {
    ["file_list", "file_read", "source_search"].contains(&name)
        && s.active_tools.contains(name)
        && !ToolRegistry::closing_withholds(s, name)
        && !ToolRegistry::recovery_withholds(s, name)
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
            let tool = call.name.clone();
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
                    Err(anyhow::anyhow!(stop_message(deadline.reason, &tool, timeout)))
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
                        s.file_cursors.insert_counted(id, cursor);
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
                        s.sources.insert_counted(id.clone(), source);
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
    s.task.migrate_legacy_plan();
    // An output that already exists when a task begins, before this task
    // saved it, is earlier work; its sections this task leaves unchanged owe
    // no reread (unread_citations).
    if s.task_rounds == 0 && !s.document_written {
        s.document_baseline = s
            .is_document_work()
            .then(|| tools::output_path(&s.project).ok())
            .flatten()
            .and_then(|path| {
                let doc = tools::read_text(&path).ok()?;
                Some((path, tools::section_fingerprints(&doc)))
            });
    }
    s.status = "running".into();
    s.last_error = None;
    // Every run brings its own budget. A resumed task starts a fresh progress
    // ladder; closing mode and its reported gaps belong to the previous run.
    s.progress_recovery.closing = None;
    s.progress_recovery.plan_checkpoints_seen = s.checkpoints_completed;
    s.progress_recovery.evidence_credit = tools::delivered_source_lines(&s);
    s.progress_recovery.best_score = progress_score(&s);
    s.progress_recovery.rounds_since_best = 0;
    s.progress_recovery.fewest_document_issues = None;
    s.progress_recovery.document_issues_hash = None;
    // The ladder counts completed model requests, so the first loop pass of
    // a run only establishes the baseline score.
    let mut ladder_request_completed = false;
    s.completion_gaps.clear();
    let mut failure = None;
    let mut finalization_attempts = s.progress_recovery.finalization_attempts;
    let mut length_recoveries = 0usize;
    let mut empty_completions = 0usize;
    // The recovery reason the last empty reply set, cleared with its error
    // once a reply carries text or calls again.
    let mut empty_reply_reason: Option<String> = None;
    // The recovery reason a rejected batch set; it describes only that
    // batch, so the next executed batch clears it (left in place, it kept
    // focused recovery on until the next document edit).
    let mut batch_reason: Option<String> = None;
    // A reply without calls whose outcome is recorded at the next request
    // or the end of the run, once the runtime has handled it.
    let mut pending_reply = false;
    // Unread cited ranges counted after the last tool batch. Only tool calls
    // change the delivered evidence or the document, so the next request
    // starts from this count instead of scanning again; an external edit in
    // between still shows in that request's readiness audit, which runs
    // afresh.
    let mut known_unread: Option<usize> = None;
    // Smoothed duration of completed model requests in this run. A slow
    // model needs a larger closing reserve than a share of the time budget.
    let mut request_secs: Option<f64> = None;
    // The model request_secs was learned with.
    let mut learned_model = s.config.model.clone();
    // Consecutive provider outages and the requests they consumed.
    let mut provider_outages = 0usize;
    let mut provider_requests = 0usize;
    let mut tool_failures = tools::recovery::FailureTracker::default();
    let mut repetitions = std::collections::BTreeMap::<String, usize>::new();
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
        if std::mem::take(&mut pending_reply) {
            settle_reply(&mut s);
        }
        consume_commands(&mut s, &mut commands);
        match apply_pending_config(&mut s, true) {
            Ok(true) => {
                s.pending_config_error = None;
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
                s.pending_config_error = Some(error.to_string());
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
        // A model switched during the run has its own pace. Relearn it, as
        // apply_config does with token calibration: the previous model's
        // slow requests would start closing at once.
        if s.config.model != learned_model {
            learned_model.clone_from(&s.config.model);
            request_secs = None;
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
        // A request expected to outlast the run would be cut at the deadline
        // and its work discarded: finish the saved document with its gaps
        // now. Half the usual duration still lets a short request run.
        if request_secs.is_some_and(|secs| (seconds_remaining as f64) < secs / 2.0)
            && let Some(text) = force_finish(&mut s, "run_timeout")
        {
            publish_final(&mut s, text, &events, &cancel, started).await;
            break;
        }
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
        let declared = declared_phase(&s.task.phase, s.is_document_work());
        if rank(declared) > rank(&phase) {
            phase = declared.to_owned();
        }
        // Chat explanations get a bounded investigation hint, not a forced finish:
        // missing evidence may still be read, while document workflows retain verification.
        // Like progress recovery below, it steers only this request. Kept in
        // the task phase it outranked every later phase: a live correction
        // run whose plan landed in round 8 was told to answer from gathered
        // evidence for the rest of its review.
        let answer_hint = finalization_attempts == 0
            && s.task_rounds >= 6
            && s.task.workflow != "source_document"
            && s.task.current_todo().is_none()
            && s.task.deliverables.is_empty()
            && !s.document_written;
        if finalization_attempts > 0 {
            // A rejected document completion returns to verification, even if
            // the model previously declared itself ready to answer; before
            // any save there is nothing to verify, and the refusal asks for
            // the document.
            phase = if s.is_document_work() && !s.document_written {
                "draft"
            } else {
                "verify"
            }
            .into();
        }
        // One progress ladder for document work: focus (1x), narrowed tools
        // (2x), then closing mode (3x) or the closing budget reserve.
        let unread_count = known_unread
            .take()
            .unwrap_or_else(|| unread_citation_count(&s));
        let request_completed = std::mem::replace(&mut ladder_request_completed, true);
        // A closing request counted here may still become a checkpoint
        // request below; that request then returns its closing round.
        let mut closing_round_counted = false;
        if s.checkpoint.is_none() {
            // New source evidence is always progress, in every phase and
            // after the first save: reading more of the project is how a
            // document gets complete. Only investigation, an unsaved
            // document, open repairs or unread citations used to count it,
            // so further reading late in a run looked like a stall. Only
            // lines not delivered before count; re-reading them or the
            // output is not new evidence.
            s.progress_recovery.evidence_credit = s
                .progress_recovery
                .evidence_credit
                .max(tools::delivered_source_lines(&s));
            let score = progress_score(&s);
            let repaired = document_issues_fell(&mut s);
            if score > s.progress_recovery.best_score {
                s.progress_recovery.best_score = score;
                s.progress_recovery.rounds_since_best = 0;
            } else if repaired {
                s.progress_recovery.rounds_since_best = 0;
            } else if request_completed {
                s.progress_recovery.rounds_since_best =
                    s.progress_recovery.rounds_since_best.saturating_add(1);
            }
            if s.is_document_work() && s.progress_recovery.closing.is_none() {
                // The time reserve also covers the closing requests at this
                // run's pace. It never exceeds the verification share, so a
                // very slow model does not close half way; the deadline
                // finishes the rest.
                let time_short = request_secs.is_some_and(|secs| {
                    let cap =
                        s.config.verification_reserve_ratio * s.config.run_timeout_secs as f64;
                    let remaining = seconds_remaining as f64;
                    remaining < cap && remaining < secs * CLOSING_REQUEST_RESERVE
                });
                let reason = if fraction <= s.config.closing_reserve_ratio || time_short {
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
                if request_completed {
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
        let persisted_phase = phase.clone();
        if answer_hint {
            phase = "answer".into();
        }
        let closing_active = s.progress_recovery.closing.is_some();
        if closing_active {
            phase = "verify".into();
        }
        let stall_rounds = s.progress_recovery.rounds_since_best;
        let document_work = s.is_document_work();
        let planned_work = s.task.current_todo().is_some();
        let focused_repair = s.progress_recovery.recovery_reason.is_some();
        let repeated_outcome_focus =
            s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit;
        let substantive_focus = s.progress_recovery.rounds_without_substantive_progress
            >= artifact_focus_limit(&s.config);
        let artifact_focus =
            s.progress_recovery.artifact_edits_without_milestone >= artifact_focus_limit(&s.config);
        let focused = s.checkpoint.is_none()
            && (focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus);
        let progress_recovery = s.checkpoint.is_none()
            && (repeated_read_detected
                || (document_work && stall_rounds >= s.config.stall_round_limit)
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
        // The focused instruction points at recovery_reason, which the
        // counters set only when they are exhausted; name their stall here.
        let shown_reason = s.progress_recovery.recovery_reason.clone().or_else(|| {
            if repeated_outcome_focus {
                Some(REPEATED_OUTCOME_REASON.into())
            } else if substantive_focus {
                Some(NAVIGATION_STALL_REASON.into())
            } else if artifact_focus {
                Some(ARTIFACT_CHURN_REASON.into())
            } else {
                None
            }
        });
        const DOCUMENT_FOCUSED_INSTRUCTION: &str = "Focused recovery: recovery_reason says what stalled. Choose the current to-do and perform one concrete action, different from the one that failed, that changes the requested result or verifies specific missing evidence; do not cycle between earlier file versions. After a real edit, advance its to-do or read any cited range it left unread.";
        const ANSWER_FOCUSED_INSTRUCTION: &str = "Focused recovery: recovery_reason says what stalled. Choose the current to-do, or the question itself, and perform one concrete action, different from the one that failed, that changes the requested result; do not cycle between earlier file versions. If the requested result already exists, complete only the actual remaining work, then answer.";
        s.run_guidance = json!({"task_rounds":s.task_rounds,"run_rounds":s.run_rounds(),"finalization_attempts":finalization_attempts,"phase":phase,"remaining_tokens":remaining,"remaining_seconds":seconds_remaining,"unread_citation_count":unread_count,
            "recovery_reason":shown_reason,"action_required":s.progress_recovery.action_required,
            "progress_recovery":{"active":progress_recovery,"focused":focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus,"rounds_without_substantive_progress":s.progress_recovery.rounds_without_substantive_progress,"repeated_outcome_rounds":s.progress_recovery.repeated_outcome_rounds,"artifact_edits_without_milestone":s.progress_recovery.artifact_edits_without_milestone,"repeated_read":repeated_read_detected},
            "current_todo":s.task.current_todo(),
            "max_tool_calls":32.min(if s.checkpoint.is_some() { ContextManager::cleanup_result_budget(&s.config) } else { s.config.batch_budget() } / 200),
            "plan_pending_count":s.task.todos.iter().filter(|item| !item.done).count(),
            "finalization_error":s.last_error.as_deref().filter(|error| final_refusal(error) && (finalization_attempts > 0 || error.starts_with("task_plan_pending:") || error.starts_with("task_unresolved_open:") || error.starts_with("request_coverage_check:")) && !(closing_active && (error.starts_with("request_coverage_check:") || (s.document_written && error.starts_with("task_plan_pending:"))))),
            "instruction":if focused_repair || repeated_outcome_focus || substantive_focus || artifact_focus { focused_instruction } else if progress_recovery { if document_work { "Progress recovery: the last requests produced no new evidence, document change or resolved citation. Change approach instead of repeating them: read sources the document still needs and has not read, write or correct a section, or file_read a cited range reported as unread." } else if planned_work { "Progress recovery: plan edits or repeated reads have not produced an outcome. Execute the first unfinished item using available evidence and tools. Do not recreate the plan or save another summary. Insert only a concrete missing prerequisite; complete an item only with the actual result. If evidence is missing, read only the necessary range." } else { "Progress recovery: repeated preparation has not produced an outcome. Correct any necessary task_plan call using its returned example, then carry out the first concrete action; otherwise answer from existing evidence. Do not repeat an unchanged call or save another summary." } } else { phase_instruction(&phase, budget_phase, answer_workflow, planned_work) } });
        if s.checkpoint.is_none() && !s.progress_recovery.checkpoint_document_edits.is_empty() {
            let hash = tools::output_path(&s.project)
                .and_then(|path| tools::read_text(&path))
                .map(|doc| tools::hash(doc.as_bytes()))
                .ok();
            s.run_guidance["checkpoint_document_edits"] = json!({
                "edits":s.progress_recovery.checkpoint_document_edits,
                "hash":hash,
                "note":"These edits were saved during the last checkpoint's cleanup, which cleared their results from context. They are already in the document: do not apply them again. Use hash as expected_hash for the next edit, or read the current text with document_inspect."
            });
        }
        s.run_guidance["progress_recovery"]["rounds_since_progress"] = json!(stall_rounds);
        s.run_guidance["progress_recovery"]["closing_after"] =
            json!(closing_stall_limit(&s.config));
        if answer_workflow {
            // Closing and citation audits belong to document work, which
            // answer never enters.
            s.run_guidance["progress_recovery"]
                .as_object_mut()
                .unwrap()
                .remove("closing_after");
            s.run_guidance
                .as_object_mut()
                .unwrap()
                .remove("unread_citation_count");
        }
        if let Some(check) = plan_check(&mut s) {
            s.run_guidance["plan_check"] = json!(check);
        }
        // Outside closing mode the model judges when the document is
        // complete: a "ready to finish" instruction as soon as the saved
        // document passed its checks and its to-dos closed let concise
        // models stop after the first sections. Readiness is still checked
        // here for its document_readiness problems and the to-do closeout.
        let ready = s.progress_recovery.closing.is_none() && ready_except_plan(&mut s);
        if ready && s.task.current_todo().is_some() {
            s.run_guidance["plan_closeout"] = plan_closeout(&s);
            // The audit passes as soon as one section cites read lines, so
            // this held from the first clean save on and replaced every
            // later instruction. A focused recovery names what stalled and
            // a draft or verify instruction says what the budget asks now:
            // both keep their place, with the closeout added. A plain stall
            // on a written document is usually the open to-dos, and the
            // investigate instruction says nothing the closeout does not,
            // so the closeout replaces those.
            if focused || (!progress_recovery && phase != "investigate") {
                let text = format!(
                    "{} {CLOSEOUT_NOTE}",
                    s.run_guidance["instruction"].as_str().unwrap_or("")
                );
                s.run_guidance["instruction"] = json!(text);
            } else {
                s.run_guidance["instruction"] = json!(PLAN_CLOSEOUT_INSTRUCTION);
            }
        } else if s.progress_recovery.closing.is_none()
            && !focused
            && s.run_guidance["document_readiness"]["structural_ok"] == false
        {
            s.run_guidance["instruction"] = json!(
                "The saved document does not pass its audit yet: resolve its unread_citation and other document_readiness issues before another final answer."
            );
        }
        if let Some(closing) = &s.progress_recovery.closing {
            s.run_guidance["closing"] = json!({"active":true,"reason":closing.reason,
                "rounds":closing.rounds,"round_limit":CLOSING_ROUND_LIMIT,
                "final_attempts":closing.final_attempts});
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
        // The request's count, kept while the request is the one counted:
        // counting a full context costs tens of milliseconds, and an
        // unchanged request was counted twice every round.
        let mut counted = Some(estimate);
        let checkpoint_was_open = s.checkpoint.is_some();
        if let Err(e) = ContextManager::prepare(&mut s, estimate) {
            failure = Some(e.to_string());
            break;
        }
        // A checkpoint that cannot bring the request within budget stops
        // the run before its cleanup requests, with the context unchanged.
        if !checkpoint_was_open
            && let Err(e) = ContextManager::check_checkpoint_fits(&s, definitions.clone())
        {
            s.checkpoint = None;
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
            counted = None;
            request = match ContextManager::request(&s, ToolRegistry::definitions(&s)) {
                Ok(r) => r,
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            };
            // input_budget reserves three cleanup rounds; later ones run
            // while they fit. Every round stayed in context, so a checkpoint
            // that needed more ended at context_limit, and each resume failed
            // at once on the same request. The oldest rounds leave context
            // instead; what they saved shows in the state.
            while ContextManager::calibrated(&s, context::count(&request, &s.config.model))
                .saturating_add(ContextManager::cleanup_output_tokens(&s.config))
                .saturating_add(512)
                > s.config.context_tokens
                && ContextManager::hide_cleanup_round(&mut s)
            {
                match ContextManager::request(&s, ToolRegistry::definitions(&s)) {
                    Ok(r) => request = r,
                    Err(e) => {
                        failure = Some(e.to_string());
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
        }
        // After an empty reply, let the model choose: some providers answer a
        // required tool choice with reasoning only and no content or calls.
        if document_work
            && s.progress_recovery.action_required
            && empty_completions == 0
            && s.checkpoint.is_none()
        {
            // A rejected final must return to a concrete tool action; a
            // subsequent final is still gated.
            request["tool_choice"] = json!("required");
            counted = None;
        }
        let raw_request_tokens =
            counted.unwrap_or_else(|| context::count(&request, &s.config.model));
        let request_tokens = ContextManager::calibrated(&s, raw_request_tokens);
        // Reasoning models spend output tokens on reasoning too. Cleanup
        // requests use the bounded cleanup allowance that input_budget reserves.
        let mut request_config = s.config.clone();
        if s.checkpoint.is_some() {
            request_config.output_tokens = ContextManager::cleanup_output_tokens(&s.config);
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
        s.activity = json!({"stage":"model","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds()});
        snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
        let deadline = run_deadline(started, &s.config);
        if tokio::time::Instant::now() >= deadline {
            if let Some(text) = force_finish(&mut s, "run_timeout") {
                publish_final(&mut s, text, &events, &cancel, started).await;
                break;
            }
            failure = Some("run_timeout: event delivery exhausted execution deadline".into());
            break;
        }
        if let Some(cp) = &mut s.checkpoint {
            cp.attempts += 1;
        }
        let document_workflow = s.is_document_work();
        request[crate::llm::STREAM_DELTAS_MARKER] = json!(!document_workflow);
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
                if document_workflow {
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
        let request_started = std::time::Instant::now();
        let response = tokio::select! {_ = cancel.cancelled()=>Err(anyhow::anyhow!("cancelled")),result=tokio::time::timeout_at(deadline,std::panic::AssertUnwindSafe(client.complete(request,&request_config,cancel.clone(),tx)).catch_unwind())=>match result{Ok(Ok(r))=>r,Ok(Err(_))=>Err(anyhow::anyhow!("model_worker_panic: model request interrupted; session retained")),Err(_)=>Err(anyhow::anyhow!("run_timeout"))}};
        relay_done.cancel();
        let _ = relay.await;
        if response.is_ok() {
            let secs = request_started.elapsed().as_secs_f64();
            request_secs = Some(request_secs.map_or(secs, |average| average * 0.7 + secs * 0.3));
        }
        let invalid_completion = response
            .as_ref()
            .ok()
            .and_then(|completion| crate::llm::validate_completion_bounds(completion).err());
        let response_error = response.as_ref().err().or(invalid_completion.as_ref());
        let recovered_batch = response_error
            .is_some_and(|error| recover_unexecuted_batch(&mut s, &error.to_string()));
        if recovered_batch {
            batch_reason = s.progress_recovery.recovery_reason.clone();
        }

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
        let response = match invalid_completion {
            Some(error) => Err(error),
            None => response,
        };
        let completion = match response {
            Ok(completion) => completion,
            Err(error) => {
                if recovered_batch {
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
                    // Another wait was left but would pass the deadline: the
                    // run is out of time, so finish like the deadline does.
                    if wait.is_some()
                        && let Some(text) = force_finish(&mut s, "run_timeout")
                    {
                        publish_final(&mut s, text, &events, &cancel, started).await;
                        break;
                    }
                    failure = Some(format!(
                        "{error}; provider unavailable after {provider_requests} requests over {} waits",
                        provider_outages
                    ));
                    break;
                }
                // The deadline fell while the request ran, as it nearly
                // always does with a slow model: finish the saved document
                // with its gaps, as the deadline check before a request does.
                // A live run otherwise ended blocked with no gap report.
                if error.to_string() == "run_timeout"
                    && let Some(text) = force_finish(&mut s, "run_timeout")
                {
                    publish_final(&mut s, text, &events, &cancel, started).await;
                    break;
                }
                failure = Some(error.to_string());
                break;
            }
        };
        if completion.calls.is_empty() {
            pending_reply = note_reply(&mut s, &completion);
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
            let reason = format!(
                "The model returned {empty_completions} empty response(s) in a row (no text and no tool call)."
            );
            if recover_document(&mut s, &reason) {
                empty_reply_reason = Some(reason);
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

        if empty_completions > 0 {
            // The empty replies are over and their error and reason described
            // only them. Left in place, a live run showed the error until its
            // final answer and sent focused-recovery guidance with every
            // request until its next document edit.
            if s.last_error
                .as_deref()
                .is_some_and(|error| error.starts_with("empty_completion:"))
            {
                s.last_error = None;
            }
            if empty_reply_reason.is_some()
                && s.progress_recovery.recovery_reason == empty_reply_reason
            {
                s.progress_recovery.recovery_reason = None;
            }
            empty_reply_reason = None;
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
            // An output this task has not written yet counts too: correcting
            // a large existing document carries the same risk.
            let document_tokens = tools::output_path(&s.project)
                .and_then(|path| tools::read_text(&path))
                .ok()
                .map_or(0, |doc| context::tokens(&doc, &s.config.model));
            if s.is_document_work() && document_tokens >= s.config.output_tokens / 4 {
                s.progress_recovery.whole_write_withheld = true;
            }
            s.activity = json!({"stage":"continuing","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds()});
            snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
            if length_recoveries >= LENGTH_RECOVERY_LIMIT {
                // The advice follows what the next edit can be: with no
                // output yet, a smaller create; with one, section edits.
                let advice = if s.progress_recovery.whole_write_withheld {
                    "edit one section per document_edit action=section or replace_text, since whole-document write is withheld for a large document"
                } else if document_tokens == 0 {
                    "create the document with its first section only, then add each other section with its own save"
                } else {
                    "save one section per document_edit call instead of rewriting the document"
                };
                let reason = format!(
                    "length_recovery_limit: {length_recoveries} consecutive output-limit responses; {advice}."
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
        let continuing = s.checkpoint.is_none() && s.continuation.is_some();
        if s.checkpoint.is_none() {
            s.continuation = None;
        }
        let batch_limit = if s.checkpoint.is_some() {
            ContextManager::cleanup_result_budget(&s.config)
        } else {
            s.config.batch_budget()
        };
        let call_limit = 32.min(batch_limit / 200);
        if completion.calls.len() > call_limit {
            let reason =
                format!("tool_call_batch_limit: maximum {call_limit} calls at this result budget");
            if recover_unexecuted_batch(&mut s, &reason) {
                batch_reason = s.progress_recovery.recovery_reason.clone();
                continue;
            }
            failure = Some(reason);
            break;
        }
        if completion.calls.is_empty() {
            let hold_document_final = document_workflow && s.checkpoint.is_none();
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
            // Closing withholds memory_manage, and its forced finish ignores
            // pending settings too, so it does not refuse for them.
            if s.pending_config.is_some() && s.progress_recovery.closing.is_none() {
                let reason = "New settings are waiting: pending_settings names what blocks them and how to resolve it";
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
            let closing_attempt = s.progress_recovery.closing.as_mut().map(|closing| {
                closing.final_attempts = closing.final_attempts.saturating_add(1);
                closing.final_attempts
            });
            let accept_gaps = s.is_document_work()
                && closing_attempt.is_some_and(|n| n >= 2)
                && document_saved(&s);
            // Closing reports open to-dos as unfinished, as its instruction
            // says; refusing its final for them only forced another round.
            let closing_reports_todos = closing_attempt.is_some() && s.document_written;
            if !accept_gaps
                && !closing_reports_todos
                && let Some(item) = s.task.current_todo()
            {
                s.last_error = Some(format!(
                    "task_plan_pending: {} ({}) is unfinished. Continue its actual work, or complete it with the observed result using task_plan if it is already done. Do not repeat completed investigation just to update the plan.",
                    item.id, item.text
                ));
                // A premature answer is a request to continue the plan, not a
                // failed evidence review. Do not spend the finalization retry
                // allowance or stop the task for unfinished plan bookkeeping.
                s.progress_recovery.repeated_outcome_rounds = s
                    .progress_recovery
                    .repeated_outcome_rounds
                    .saturating_add(1);
                s.run_guidance["progress_recovery"]["repeated_outcome_rounds"] =
                    json!(s.progress_recovery.repeated_outcome_rounds);
                s.run_guidance["progress_recovery"]["active"] = json!(
                    repeated_read_detected
                        || s.progress_recovery.repeated_outcome_rounds
                            >= s.config.stall_round_limit
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
                // A live correction was told to create a document that
                // existed, after completing its to-dos without one edit.
                s.last_error = Some(if output_exists(&s) {
                    "The requested source document has not been saved in this task: it existed before the task and is unchanged, so none of the corrections the request asks for is in it. Save them with document_edit or document_edit_batch (replace_text or section; the document exists, so not create) before finishing".into()
                } else {
                    "The requested source document has not been saved in this task: create it with document_edit (action=create) from the evidence already gathered before finishing".into()
                });
            } else if s.is_document_work() {
                match tools::audit_document(&mut s) {
                    Ok(audit) if audit["structural_ok"] == true => {
                        // A source document cites the sources of its claims.
                        // Without a citation nothing ties them to the source:
                        // a live run finished "complete" with no source check
                        // and no reported gap. The final answer goes back to
                        // work; closing accepts it and reports the omission.
                        if tools::citation_count(&s) == 0 && !accept_gaps {
                            s.status = "partial".into();
                            s.last_error = Some(DOCUMENT_CITATIONS_MISSING.into());
                        } else {
                            s.status = "complete".into();
                            s.last_error = None;
                        }
                    }
                    Ok(audit) => {
                        s.status = "partial".into();
                        s.last_error = Some(format!(
                            "Document format/evidence audit has {} issues; see run_guidance.document_readiness",
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
                }
                s.status = "running".into();
                emit(
                    &events,
                    AgentEvent::Notice {
                        session: s.id.clone(),
                        text: finalization_notice(s.last_error.as_deref()).into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                continue;
            }
            // The first final of a task compares the document with the
            // request once; closing mode finishes with what it has.
            if s.status == "complete"
                && s.is_document_work()
                && s.progress_recovery.closing.is_none()
                && let Some(error) = coverage_check(&mut s)
            {
                // Not action_required: its forced tool call would rule out
                // the answer "every part is covered".
                s.status = "running".into();
                s.last_error = Some(error);
                emit(
                    &events,
                    AgentEvent::Notice {
                        session: s.id.clone(),
                        text: "Checking that the document covers the whole request before the final answer.".into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                continue;
            }
            // An accepted final reports task.unresolved as gaps: show the
            // items once so resolved ones are dropped before they are.
            if s.status == "complete"
                && s.is_document_work()
                && !accept_gaps
                && let Some(error) = unresolved_closeout(&mut s)
            {
                // Not action_required: keeping every item and answering
                // again is a valid reply, which a forced tool call rules out.
                s.status = "running".into();
                s.last_error = Some(error);
                emit(
                    &events,
                    AgentEvent::Notice {
                        session: s.id.clone(),
                        text: "Checking the unresolved items before the final answer.".into(),
                    },
                    &cancel,
                    run_deadline(started, &s.config),
                )
                .await;
                continue;
            }
            let mut final_text = completion.text.clone();
            if s.status == "complete" && s.is_document_work() {
                s.completion_gaps = collect_gaps(&mut s);
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
            s.progress_recovery.action_required = false;
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
        let prior_source_lines = tools::delivered_source_lines(&s);
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
            let parallel = parallel_read(&s, &call.name)
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
                    && parallel_read(&s, &execution_calls[i + group].name)
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
            let steps: Vec<String> = execution_calls[i..i + group]
                .iter()
                .map(|c| tools::call_summary(&s.project.root, &c.name, &c.arguments))
                .collect();
            s.activity = json!({"stage":"tools","started_at_ms":chrono::Utc::now().timestamp_millis(),"round":s.run_rounds(),"tools":execution_calls[i..i+group].iter().map(|c| c.name.clone()).collect::<Vec<_>>(),"steps":&steps});
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
                    s.remember_call(
                        call.id.clone(),
                        format!("{}:{}", call.name, call.arguments),
                        result.clone(),
                    );
                }
                vec![result]
            };
            for ((call, mut result), summary) in execution_calls[i..i + group]
                .iter()
                .zip(results)
                .zip(steps.iter().cloned())
            {
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
                        tools::recovery::annotate_identical_document_failure(&s, &mut result);
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
                // A checkpoint's commit clears the results of edits saved
                // during its cleanup; list them afterwards so they are not
                // applied twice. A later edit or inspection has the hash.
                if result["status"] == "ok"
                    && matches!(
                        call.name.as_str(),
                        "document_edit" | "document_edit_batch" | "document_inspect"
                    )
                {
                    if s.checkpoint.is_some() && call.name != "document_inspect" {
                        let edits = &mut s.progress_recovery.checkpoint_document_edits;
                        if edits.len() < CHECKPOINT_EDITS_LISTED {
                            edits.push(describe_document_edit(&call.name, &call.arguments));
                        }
                    } else if s.checkpoint.is_none() {
                        s.progress_recovery.checkpoint_document_edits.clear();
                    }
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

                // The tool run marks the failures that fail the checkpoint
                // batch (tools::failure_holds_checkpoint).
                if result["status"] != "ok"
                    && let Some(cp) = &mut s.checkpoint
                    && cp.failed
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
                    .min(s.config.result_budget())
                    .max(200);
                let result = tools::limit_result(&mut s, call, result, budget);
                let result = suppress_unchanged_repeat(&s, &messages, call, result);
                tools::record_delivered_read(&mut s, call, &result);
                if result["status"] == "ok"
                    && ["file_list", "source_search", "document_inspect", "db_query"]
                        .contains(&call.name.as_str())
                    && result["data"]["suppressed"] != true
                {
                    novel_navigation_evidence |= s
                        .progress_recovery
                        .remember_navigation(&call.name, &result["data"]);
                }
                if cache_parallel_result {
                    // Parallel reads run in temporary sessions, so their
                    // run_call ledger entries cannot be merged safely until
                    // source IDs and archive/cursor IDs have been remapped.
                    // Cache the final owner-session representation so a
                    // provider replaying the same call ID remains idempotent.
                    s.remember_call(
                        call.id.clone(),
                        format!("{}:{}", call.name, call.arguments),
                        result.clone(),
                    );
                }
                remaining =
                    remaining.saturating_sub(tools::result_tokens(call, &result, &s.config.model));
                let status = result["status"].as_str().unwrap_or("error").to_owned();
                emit(
                    &events,
                    AgentEvent::Tool {
                        session: s.id.clone(),
                        name: call.name.clone(),
                        status,
                        summary,
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
        // Document edits close after a checkpoint's first cleanup batch
        // (tools::ToolRegistry::checkpoint_writes_open).
        if checkpoint_batch && let Some(cp) = &s.checkpoint {
            let batches = &mut s.progress_recovery.checkpoint_write_batches;
            if batches.0 != cp.id {
                *batches = (cp.id.clone(), 0);
            }
            batches.1 += 1;
        }
        // Reading a cited range the document still owed counts like a
        // verification did: the saved result is better supported.
        // Only the tool batch changes the delivered evidence or the document
        // within a request, so the count at its start is the count before
        // this batch. The final answer audits afresh either way.
        let unread_after = unread_citation_count(&s);
        let verified_progress = unread_after < unread_count;
        known_unread = Some(unread_after);
        if verified_progress {
            repetitions.clear();
        }
        let new_source_evidence = tools::delivered_source_lines(&s) > prior_source_lines;
        let pending_todos = s.task.todos.iter().filter(|item| !item.done).count();
        let plan_advanced = prior_current_todo.is_some()
            && pending_todos < prior_pending_todos
            && s.task.current_todo().map(|item| &item.id) != prior_current_todo.as_ref();
        // Edits grounded in newly read sources are progress too, even when
        // they refine a section without adding lines.
        if verified_progress || plan_advanced || artifact_milestone || new_source_evidence {
            s.progress_recovery.artifact_edits_without_milestone = 0;
        } else if novel_artifact_change && !checkpoint_batch {
            s.progress_recovery.artifact_edits_without_milestone = s
                .progress_recovery
                .artifact_edits_without_milestone
                .saturating_add(1);
        }
        if !s.is_document_work() {
            // Outside document work the reason only explains the rejected batch.
            s.progress_recovery.recovery_reason = None;
        }
        if batch_reason.is_some() && s.progress_recovery.recovery_reason == batch_reason {
            s.progress_recovery.recovery_reason = None;
        }
        batch_reason = None;
        // No separate count of requests without an edit or a resolved
        // citation: reading new sources is progress too, and that count sent
        // a live run toward writing after eight requests of new reads. The
        // progress ladder, repeated-outcome and repeated-read checks catch
        // real stalls.
        if novel_artifact_change || verified_progress {
            s.progress_recovery.recovery_reason = None;
            repeated_read_detected = false;
            finalization_attempts = 0;
            s.progress_recovery.finalization_attempts = 0;
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
        s.progress_recovery.repeated_read = repeated_read_detected;
        s.run_guidance["progress_recovery"] = json!({
            "active":s.checkpoint.is_none() && (repeated_read_detected
                || s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit),
            "focused":s.progress_recovery.repeated_outcome_rounds >= s.config.stall_round_limit
                || s.progress_recovery.rounds_without_substantive_progress >= artifact_focus_limit(&s.config)
                || s.progress_recovery.artifact_edits_without_milestone >= artifact_focus_limit(&s.config),
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
        if !checkpoint_batch
            && repeated_outcome_exhausted(&s)
            && !recover_document(&mut s, REPEATED_OUTCOME_REASON)
        {
            s.status = "partial".into();
            s.last_error = Some(REPEATED_OUTCOME_ERROR.into());
            break;
        }
        if !checkpoint_batch
            && s.progress_recovery.rounds_without_substantive_progress
                >= substantive_progress_limit(&s.config)
            && !recover_document(&mut s, NAVIGATION_STALL_REASON)
        {
            s.status = "partial".into();
            s.last_error = Some(NAVIGATION_STALL_ERROR.into());
            break;
        }
        if !checkpoint_batch
            && s.progress_recovery.artifact_edits_without_milestone
                >= artifact_exhaust_limit(&s.config)
            && !recover_document(&mut s, ARTIFACT_CHURN_REASON)
        {
            s.status = "partial".into();
            s.last_error = Some(ARTIFACT_CHURN_ERROR.into());
            break;
        }
        // The batch is over: its progress line must not still read as
        // running while its results already show in the history. A failure
        // set in the batch ends the run here, and the run history records
        // the stage it ended in, so that stage stays.
        if failure.is_none() {
            s.activity =
                json!({"stage":"preparing","started_at_ms":chrono::Utc::now().timestamp_millis()});
        }
        snapshot(&s, &events, &cancel, run_deadline(started, &s.config)).await;
    }
    if pending_reply {
        settle_reply(&mut s);
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
                AgentEvent::Tool {
                    name,
                    status,
                    summary,
                    ..
                } => (
                    console::Target::Stderr,
                    format!("\n[{name}: {status}] {summary}\n"),
                ),
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
mod parallel_read_tests {
    use super::*;

    // Parallel workers run on a temporary session without closing or
    // progress-recovery state; a read either withholds must not go there.
    #[test]
    fn withheld_reads_do_not_run_on_parallel_workers() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            crate::config::Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            crate::config::Config::compact_test(),
        );
        s.active_tools = ToolRegistry::optional_names();
        s.task.workflow = "source_document".into();
        assert!(parallel_read(&s, "source_search"));
        s.document_written = true;
        s.progress_recovery.rounds_since_best = s.config.stall_round_limit * 2;
        assert!(!parallel_read(&s, "source_search"));
        assert!(parallel_read(&s, "file_read"));
        s.progress_recovery.rounds_since_best = 0;
        s.progress_recovery.closing = Some(Default::default());
        assert!(!parallel_read(&s, "file_list"));
    }
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
    if copy.config.model != config.model {
        // Calibration samples describe the previous model's tokenizer.
        copy.token_ratios.clear();
    }
    copy.config = config;
    ContextManager::state(&copy)?;
    let request = ContextManager::request(&copy, ToolRegistry::definitions(&copy))?;
    // Sized as the run loop checks the next request: calibrated, with its
    // margin. A raw count let a smaller context through whose next request,
    // a pending checkpoint's cleanup included, the run then refused with
    // context_limit on every resume.
    if ContextManager::calibrated(&copy, context::count(&request, &copy.config.model))
        .saturating_add(copy.config.output_tokens)
        .saturating_add(512)
        > copy.config.context_tokens
    {
        bail!("New context budget requires checkpoint first; current settings retained");
    }
    s.config = copy.config;
    s.token_ratios = copy.token_ratios;
    Ok(())
}

#[cfg(test)]
mod plan_check_tests {
    use super::*;
    use crate::config::Project;

    #[test]
    fn phase_guidance_matches_the_workflow_and_where_the_phase_came_from() {
        // A declared answer stage steers document work as verification; the
        // chat answer instruction stays with the answer workflow.
        assert_eq!(declared_phase("answer", true), "verify");
        assert_eq!(declared_phase("answer", false), "answer");
        assert_eq!(declared_phase("draft", true), "draft");
        // A draft phase kept from an earlier run is not attributed to a
        // declaration or to the budget of this run.
        let kept = phase_instruction("draft", "investigate", false, false);
        assert!(kept.starts_with("Draft phase"), "{kept}");
        assert!(
            !kept.contains("declared") && !kept.contains("budget"),
            "{kept}"
        );
        let budget = phase_instruction("draft", "draft", false, false);
        assert!(
            budget.starts_with("Half of the run budget is spent"),
            "{budget}"
        );
        for phase in ["investigate", "draft", "verify"] {
            let text = phase_instruction(phase, "investigate", false, false);
            assert!(!text.contains("Answer the user now"), "{phase}: {text}");
        }
        assert!(
            phase_instruction("investigate", "investigate", false, false)
                .contains("when the document is complete")
        );
        // A phase instruction says what this phase changes; the standing
        // rules are in the system prompt, sent with the same request.
        for (phase, budget, answer) in [
            ("investigate", "investigate", false),
            ("investigate", "investigate", true),
            ("draft", "draft", false),
            ("draft", "investigate", false),
            ("verify", "verify", false),
            ("answer", "investigate", true),
        ] {
            let prompt = if answer {
                crate::context::ANSWER_SYSTEM.as_str()
            } else {
                crate::context::SYSTEM.as_str()
            };
            for sentence in phase_instruction(phase, budget, answer, false)
                .split(". ")
                .map(|sentence| sentence.trim_end_matches('.'))
                .filter(|sentence| sentence.len() >= 40)
            {
                assert!(!prompt.contains(sentence), "{phase}: {sentence}");
            }
        }
        // An open to-do refuses the final answer, so the answer phase does
        // not tell the model to answer now while one is open.
        let open = phase_instruction("answer", "investigate", true, true);
        assert!(!open.contains("Answer the user now"), "{open}");
        assert!(open.contains("open to-dos"), "{open}");
        assert!(
            phase_instruction("answer", "investigate", true, false).contains("Answer the user now")
        );
        // The final answer's contents are listed once, in the prompt.
        assert!(!READY_FOR_FINAL_INSTRUCTION.contains("verification scope"));
        // finalization_error explains a refused final answer; a batch-limit
        // or empty-reply error set later in the same stretch is not one.
        assert!(final_refusal(DOCUMENT_CITATIONS_MISSING));
        assert!(final_refusal("task_plan_pending: 2 to-dos are open"));
        for other in [
            "batch_limit: 40 calls exceed max_tool_calls 32",
            "model_empty_reply: no text and no tool call",
        ] {
            assert!(!final_refusal(other), "{other}");
        }
    }

    #[test]
    fn closing_keeps_unfinished_to_dos_open_for_the_gap_report() {
        // The closing result lists open to-dos as unfinished; a removed to-do
        // leaves that list, so closing must not ask to remove undone work.
        let mut s = Session::new(Project::default(), Config::compact_test());
        s.select_workflow("source_document").unwrap();
        s.document_written = true;
        s.progress_recovery.closing = Some(crate::session::Closing {
            reason: "stall".into(),
            ..Default::default()
        });
        let text = closing_instruction(&s);
        assert!(text.contains("leave unfinished ones open"), "{text}");
        assert!(!text.contains("Complete or remove remaining"), "{text}");
        // The audit's issues reach the model as document_readiness.
        assert!(!text.contains("from document_audit"), "{text}");
    }

    #[test]
    fn a_stale_to_do_is_pointed_out_after_a_saved_section_and_once_per_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config {
                stall_round_limit: 3,
                ..Config::compact_test()
            },
        );
        s.select_workflow("source_document").unwrap();
        s.add_user("Write a source document".into());
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Write the usage section","Verify the document"]}]}),
        )
        .unwrap();
        let mut checks = vec![];
        for round in 1..=8 {
            s.task_rounds = round;
            if round == 2 {
                s.progress_recovery.best_document_section_count = 1;
            }
            if round == 6 {
                s.checkpoints_completed = 1;
            }
            checks.push(plan_check(&mut s));
        }
        let shown: Vec<_> = checks
            .iter()
            .enumerate()
            .filter_map(|(i, check)| check.as_ref().map(|text| (i + 1, &text[..20])))
            .collect();
        // The saved section used to keep its own check true while T1 stayed
        // open, hiding the stale check for 25 requests of a live run. The
        // stale state is not shown again after the checkpoint check.
        assert_eq!(
            shown,
            [
                (1, "Before reading furth"),
                (2, "A section was saved "),
                (5, "current_todo T1 has "),
                (6, "A checkpoint just co"),
            ]
        );
        // The count still starts when T1 became current.
        assert!(
            checks[4]
                .as_ref()
                .unwrap()
                .contains("current for 4 requests")
        );
    }

    #[test]
    fn a_new_plan_is_compared_with_the_request_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config::compact_test(),
        );
        s.select_workflow("source_document").unwrap();
        s.add_user("Explain the procedure from question to final completion".into());
        // No plan yet: nothing to compare until the empty-plan check is due.
        s.task_rounds = 1;
        assert_eq!(plan_check(&mut s), None);
        // The live shape: module reading items, then one section per module.
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Understand main.rs","Write document: Session Management"]}]}),
        )
        .unwrap();
        let check = plan_check(&mut s).unwrap();
        assert!(check.contains("latest_request"), "{check}");
        assert!(check.contains("a flow or procedure"), "{check}");
        assert!(
            check.contains("Items: T1 Understand main.rs; T2 Write document: Session Management"),
            "{check}"
        );
        // Shown once: a restructured plan is not asked about again.
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":1,"operations":[{"op":"update","id":"T1","text":"Receive and route the question (session.rs, question/routing.rs)"}]}),
        )
        .unwrap();
        assert_eq!(plan_check(&mut s), None);
        // The empty-plan check names steps before files.
        let mut empty = Session::new(s.project.clone(), Config::compact_test());
        empty.select_workflow("source_document").unwrap();
        empty.add_user("Explain the procedure".into());
        empty.task_rounds = PLAN_CHECK_START;
        let check = plan_check(&mut empty).unwrap();
        assert!(
            check.contains("one per step in the order it happens"),
            "{check}"
        );
    }

    #[test]
    fn a_checkpoint_before_the_first_save_names_no_saved_document() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config::compact_test(),
        );
        s.select_workflow("source_document").unwrap();
        s.add_user("Write a source document".into());
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Read the request flow","Write the document"]}]}),
        )
        .unwrap();
        s.task_rounds = 1;
        // The new plan is compared with the request once, first.
        assert!(
            plan_check(&mut s)
                .unwrap()
                .starts_with("Before reading further")
        );
        assert_eq!(plan_check(&mut s), None);
        s.checkpoints_completed = 1;
        let before_save = plan_check(&mut s).unwrap();
        assert!(
            before_save.contains("no document is saved yet"),
            "{before_save}"
        );
        assert!(!before_save.contains("the saved document"), "{before_save}");
        s.document_written = true;
        s.checkpoints_completed = 2;
        let after_save = plan_check(&mut s).unwrap();
        assert!(
            after_save.contains("against the saved document"),
            "{after_save}"
        );
    }

    // Each checkpoint of a long task used to leave its key behind.
    #[test]
    fn the_plan_checks_shown_keep_one_key_of_each_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config::compact_test(),
        );
        s.select_workflow("source_document").unwrap();
        s.add_user("Write a source document".into());
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Read the request flow","Write the document"]}]}),
        )
        .unwrap();
        s.task_rounds = 1;
        assert!(plan_check(&mut s).is_some());
        for checkpoint in 1..=200 {
            s.checkpoints_completed = checkpoint;
            assert!(plan_check(&mut s).is_some(), "{checkpoint}");
            // The same state is still pointed out once.
            assert_eq!(plan_check(&mut s), None);
        }
        assert_eq!(
            s.progress_recovery.plan_checks_shown,
            ["checkpoint:200".to_owned(), PLAN_STRUCTURE_KEY.to_owned()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn unresolved_items_shown_are_only_the_current_ones() {
        let mut s = Session::new(Project::default(), Config::compact_test());
        s.task.unresolved = vec!["A".into()];
        assert!(unresolved_closeout(&mut s).is_some());
        assert_eq!(unresolved_closeout(&mut s), None);
        for item in 0..100 {
            s.task.unresolved = vec!["A".into(), format!("item {item}")];
            assert!(unresolved_closeout(&mut s).is_some());
        }
        assert_eq!(unresolved_closeout(&mut s), None);
        assert_eq!(s.progress_recovery.unresolved_shown.len(), 2);
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use crate::config::Project;

    // Live run 2026-10-13: 14 saves repaired citations over 24 requests and
    // the run was closed as stalled, because repairs add no section, line
    // or evidence.
    #[test]
    fn fewer_document_issues_than_ever_before_count_as_progress() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
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
        tools::execute(
            &mut s,
            "file_read",
            json!({"path":"a.rs","start_line":1,"max_lines":1}),
        )
        .unwrap();
        // No document yet, then the first count is the baseline.
        assert!(!document_issues_fell(&mut s));
        let save = |s: &mut Session, text: &str| {
            let args = match std::fs::read(dir.path().join("out.md")) {
                Ok(old) => {
                    json!({"action":"write","expected_hash":tools::hash(&old),"text":text})
                }
                Err(_) => json!({"action":"create","text":text}),
            };
            tools::execute(s, "document_edit", args).unwrap();
        };
        // Two citations past the end of a.rs.
        save(&mut s, "# A\n\nOne a.rs:5.\nTwo a.rs:6.\n");
        assert!(!document_issues_fell(&mut s));
        assert_eq!(s.progress_recovery.fewest_document_issues, Some(2));
        // An unchanged document is not counted again.
        assert!(!document_issues_fell(&mut s));
        save(&mut s, "# A\n\nOne a.rs:1.\nTwo a.rs:6.\n");
        assert!(document_issues_fell(&mut s));
        // Breaking a citation and repairing it again is not new progress.
        save(&mut s, "# A\n\nOne a.rs:7.\nTwo a.rs:6.\n");
        assert!(!document_issues_fell(&mut s));
        save(&mut s, "# A\n\nOne a.rs:1.\nTwo a.rs:6.\n");
        assert!(!document_issues_fell(&mut s));
        save(&mut s, "# A\n\nOne a.rs:1.\nTwo a.rs:1.\n");
        assert!(document_issues_fell(&mut s));
        assert_eq!(s.progress_recovery.fewest_document_issues, Some(0));
        // Live run 2026-10-13: a claim naming code its cited lines never
        // mention counts too, so its saved fix is progress (counted from a
        // new task's baseline).
        s.progress_recovery.fewest_document_issues = None;
        s.progress_recovery.document_issues_hash = None;
        save(&mut s, "# A\n\n`missing_fn` runs a.rs:1.\n");
        assert!(!document_issues_fell(&mut s));
        save(&mut s, "# A\n\n`a()` runs a.rs:1.\n");
        assert!(document_issues_fell(&mut s));
    }

    #[test]
    fn unread_citations_block_readiness_until_read() {
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
        // The passing audit is reported, so no document_audit call is needed
        // to confirm it (3.2 confirmation audits per live run).
        assert_eq!(
            s.run_guidance["document_readiness"],
            json!({"structural_ok":true,"citations_checked":2})
        );
        // A changed source makes the read stale and blocks the ready instruction.
        std::fs::write(dir.path().join("a.rs"), "fn changed() {}\nfn b() {}\n").unwrap();
        assert!(!ready_for_final(&mut s));
        assert_eq!(s.run_guidance["document_readiness"]["structural_ok"], false);
    }

    #[test]
    fn closing_with_an_open_to_do_still_reports_document_readiness() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/agent.rs"), "fn a() {}\n").unwrap();
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
        s.add_user("Document agent.rs.".into());
        s.select_workflow("source_document").unwrap();
        tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Write the document"]}]}),
        )
        .unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create","text":"# Manual\n\nStarts here.\n"}),
        )
        .unwrap();
        // A save completes a citation missing its folder; stand in for a
        // document whose citation still lacks it (one this task has not
        // saved), which keeps its ready fix.
        let unsaved = "# Manual\n\nStarts at agent.rs:1.\n";
        std::fs::write(dir.path().join("out.md"), unsaved).unwrap();
        s.last_document_write = Some((dir.path().join("out.md"), tools::hash(unsaved.as_bytes())));
        // A live closing kept its last to-do open and never received the
        // document's problems.
        s.progress_recovery.closing = Some(Default::default());
        assert!(!ready_for_final(&mut s));
        let readiness = &s.run_guidance["document_readiness"];
        assert_eq!(readiness["structural_ok"], false, "{readiness}");
        assert_eq!(readiness["issue_kinds"]["citation_path"], 1, "{readiness}");
        assert_eq!(
            readiness["issues"][0]["fix"],
            json!({"action":"replace_text","old_text":"agent.rs:1","text":"src/agent.rs:1"})
        );
        let instruction = closing_instruction(&s);
        assert!(instruction.contains("citation_path"), "{instruction}");
    }

    #[test]
    fn finalization_notices_name_their_cause() {
        for (error, start) in [
            (
                Some("Document format/evidence audit has 2 issues; use document_audit"),
                "The document audit found problems",
            ),
            (
                Some("Document audit failed: io"),
                "The document audit found problems",
            ),
            (
                Some("The requested source document has not been saved in this task"),
                "The requested document is not saved",
            ),
            (Some("task_plan_pending: T1"), "Final checks failed"),
            (None, "Final checks failed"),
        ] {
            assert!(finalization_notice(error).starts_with(start), "{error:?}");
        }
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
        s.config.result_tokens = Some(700);
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

    #[test]
    fn a_search_stopped_at_the_tool_time_limit_is_told_to_narrow_its_scope() {
        // A bare tool_timeout was classified as transient, which invites the
        // same wide search again.
        let message = stop_message("tool_timeout", "source_search", 30);
        assert!(message.starts_with("scope_timeout: source_search did not finish within the 30s"));
        let recovery = tools::recovery::describe(&message);
        assert_eq!(recovery["code"], "scope_timeout");
        assert_eq!(recovery["action"], "reduce_request_or_cleanup");
        // Other tools and the run deadline keep their reason.
        assert_eq!(
            stop_message("tool_timeout", "file_read", 30),
            "tool_timeout"
        );
        assert_eq!(
            stop_message("run_timeout", "source_search", 30),
            "run_timeout"
        );
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
mod usage_tests {
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

    /// Fails its first requests as `failure` says, then gives the final answer.
    struct Model {
        failure: Failure,
        requests: AtomicUsize,
        reserved_outputs: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for Model {
        async fn complete(
            &self,
            _: Value,
            config: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
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
                Failure::Fatal => anyhow::bail!("test_model_failure"),
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
            Ok(Completion {
                text: "Saved source document.".into(),
                usage: Some(Usage {
                    input: 13,
                    output: 5,
                    cached: None,
                }),
                ..Default::default()
            })
        }
    }

    /// A saved, cited and read document whose next final answer completes.
    async fn run(failure: Failure) -> (Session, Arc<Model>) {
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
        // These tests count requests and their charges; the one coverage
        // check before the first final is covered elsewhere.
        session.progress_recovery.coverage_checked = true;
        let model = Arc::new(Model {
            failure,
            requests: AtomicUsize::new(0),
            reserved_outputs: Mutex::new(Vec::new()),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(session, model.clone(), CancellationToken::new(), tx).await;
        drain.await.unwrap();
        (result, model)
    }

    #[tokio::test]
    async fn failed_requests_are_charged_before_stopping() {
        let (session, _) = run(Failure::Fatal).await;
        assert_eq!(session.status, "blocked");
        assert!(session.input_tokens > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn outage_retries_and_success_are_charged_once() {
        let (session, model) = run(Failure::Provider(2)).await;
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(model.requests.load(Ordering::SeqCst), 3);
        assert!(session.input_tokens > 13);
        assert_eq!(session.output_tokens, 5);
    }

    #[tokio::test(start_paused = true)]
    async fn refused_attempts_are_not_charged_as_input() {
        // Live run 2026-10-06: four 429 sequences added ~0.5M unbilled input
        // to the run budget. A refusal with an HTTP status generated nothing.
        let (session, model) = run(Failure::Refused(2)).await;
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(model.requests.load(Ordering::SeqCst), 3);
        assert_eq!((session.input_tokens, session.output_tokens), (13, 5));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_outages_include_every_failed_attempt() {
        let (session, model) = run(Failure::Provider(usize::MAX)).await;
        assert_eq!(session.status, "blocked");
        assert_eq!(model.requests.load(Ordering::SeqCst), 5);
        assert!(session.input_tokens > 0);
    }

    #[tokio::test]
    async fn malformed_response_recovery_charges_reserved_output() {
        let (session, model) = run(Failure::Malformed).await;
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(model.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            session.output_tokens,
            model.reserved_outputs.lock().unwrap()[0] + 5
        );
    }

    #[tokio::test]
    async fn invalid_completion_keeps_its_usage_receipt() {
        let (session, model) = run(Failure::InvalidCompletion(true)).await;
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(model.requests.load(Ordering::SeqCst), 2);
        assert_eq!((session.input_tokens, session.output_tokens), (30, 28));
    }

    #[tokio::test]
    async fn invalid_completion_without_usage_charges_reserved_output() {
        let (session, model) = run(Failure::InvalidCompletion(false)).await;
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(model.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            session.output_tokens,
            model.reserved_outputs.lock().unwrap()[0] + 5
        );
    }
}
