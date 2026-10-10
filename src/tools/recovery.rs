//! Shared recovery contract. Classification guides the model; it never retries
//! writes, guesses source IDs, or relaxes validation on the model's behalf.
use super::*;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

/// Structured failure details survive the common envelope and batch nesting.
/// Display retains the stable prefix used by existing recovery classification.
#[derive(Debug)]
pub(crate) struct DiagnosticError {
    pub message: String,
    pub data: Value,
}

impl std::fmt::Display for DiagnosticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiagnosticError {}

pub(super) fn batch_result(item: &Value) -> &Value {
    item.get("result")
        .filter(|value| value.is_object())
        .unwrap_or(item)
}

pub(super) fn is_failure(result: &Value) -> bool {
    matches!(
        result["status"].as_str(),
        Some("error" | "cancelled" | "unsupported")
    )
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Class {
    InvalidInput,
    Prerequisite,
    MissingPath,
    MissingEvidence,
    StaleState,
    Capacity,
    Unavailable,
    PartialFailure,
    Cancelled,
    Transient,
    OutcomeUnknown,
    Unclassified,
}

/// The leading snake_case code of an error message. A code may end at ':'
/// or at ';' (wrappers append "; operation_index=0" to a bare code).
pub fn error_code(message: &str) -> &str {
    let prefix = message.split([':', ';']).next().unwrap_or("");
    if !prefix.is_empty()
        && prefix.len() <= 80
        && prefix.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        prefix
    } else {
        "tool_error"
    }
}

/// Unknown errors default to inspection, never to an assumed safe retry.
pub fn describe(message: &str) -> Value {
    let code = error_code(message);
    let (class, action) = if code == "cancelled" {
        (Class::Cancelled, "stop")
    } else if matches!(code, "tool_worker_capacity" | "tool_worker_start_failed") {
        // The worker thread never started, so the call did not run.
        (Class::Unavailable, "wait_for_tool_workers")
    } else if matches!(
        code,
        "tool_worker_panic" | "tool_worker_unresolved" | "tool_batch_aborted"
    ) {
        (Class::OutcomeUnknown, "inspect_outcome_before_retry")
    } else if code == "batch_partial_failure" {
        (Class::PartialFailure, "repair_failed_items_only")
    } else if code == "checkpoint_has_failed_operations" {
        (Class::Prerequisite, "repair_checkpoint_on_next_request")
    } else if code == "no_checkpoint" {
        (Class::Prerequisite, "inspect_checkpoint_state")
    } else if matches!(
        code,
        "checkpoint_id_mismatch" | "checkpoint_not_confirmed" | "incomplete_group"
    ) {
        (Class::Prerequisite, "repair_checkpoint_on_next_request")
    } else if code == "memory_sources_required" {
        (Class::MissingEvidence, "restore_memory_evidence")
    } else if code == "memory_revision_missing" {
        (Class::InvalidInput, "supply_memory_revision")
    } else if code == "memory_revision_unexpected" {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "memory_reference_unverified" {
        (Class::Prerequisite, "repair_memory_references")
    } else if matches!(
        code,
        "memory_reference_stale" | "memory_reference_missing" | "memory_reference_mixed"
    ) {
        (Class::StaleState, "repair_memory_references")
    } else if code == "verification_sources_required" {
        (Class::MissingEvidence, "lookup_observed_evidence")
    } else if code == "file_not_found" || code == "document_missing" {
        (Class::MissingPath, "resolve_path")
    } else if code == "file_patch_rollback_failed" || code == "file_patch_write_failed" {
        (Class::OutcomeUnknown, "inspect_outcome_before_retry")
    } else if code == "file_hash_required" {
        (Class::InvalidInput, "copy_file_hash")
    } else if code == "configured_output_requires_document_edit" {
        (Class::InvalidInput, "use_document_editor")
    } else if matches!(
        code,
        "file_exists" | "empty_old_text" | "text_not_found" | "ambiguous_text"
    ) {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "file_permission_denied" {
        (Class::Unavailable, "check_file_permissions")
    } else if code == "path_is_directory" {
        (Class::InvalidInput, "select_file_from_directory")
    } else if matches!(
        code,
        "unsupported_binary_file" | "unsupported_large_file" | "unsupported_file_type"
    ) {
        // The tool works; this file cannot be read as text. Pick another.
        (Class::Unavailable, "choose_allowed_path")
    } else if matches!(
        code,
        "path_outside_project"
            | "path_excluded"
            | "output_path_escape"
            | "output_symlink_escape"
            | "parent_traversal_not_allowed"
    ) {
        (Class::InvalidInput, "choose_allowed_path")
    } else if code == "document_hash_required" {
        (Class::InvalidInput, "copy_document_hash")
    } else if code == "document_audit_revision_required" {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "document_revision_conflict" {
        (Class::StaleState, "restart_document_inspection")
    } else if code == "document_batch_operation_failed" {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "ambiguous_section" {
        (Class::InvalidInput, "choose_exact_section")
    } else if code == "invalid_citation_range" {
        (Class::InvalidInput, "repair_document_citation")
    } else if matches!(code, "section_not_found" | "document_exists") {
        (Class::InvalidInput, "inspect_document_outline")
    } else if code == "patch_target_must_match_once" {
        (Class::InvalidInput, "correct_arguments")
    } else if matches!(
        code,
        "invalid_output" | "invalid_output_parent" | "output_path_is_directory"
    ) {
        (Class::InvalidInput, "choose_allowed_path")
    } else if matches!(
        code,
        "memory_key_conflict"
            | "cursor_arguments_conflict"
            | "conflicting_arguments"
            | "ambiguous_file_read_range"
    ) {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "unknown_source" {
        (Class::MissingEvidence, "lookup_observed_evidence")
    } else if code == "unknown_symbol" || code == "invalid_symbol_id" {
        (Class::InvalidInput, "copy_observed_symbol_id")
    } else if matches!(
        code,
        "database_disabled" | "database_password_missing" | "database_execution_disabled"
    ) {
        (Class::Prerequisite, "configure_database_in_settings")
    } else if code == "database_query_disabled_or_unknown" {
        (Class::InvalidInput, "select_enabled_database_query")
    } else if code == "verification_reserve" {
        (Class::Prerequisite, "complete_prerequisite")
    } else if matches!(
        code,
        "invalid_database_query_arguments" | "invalid_database_execution_arguments"
    ) {
        (Class::InvalidInput, "correct_arguments")
    } else if matches!(
        code,
        "database_commit_uncertain" | "database_rollback_uncertain"
    ) {
        (Class::OutcomeUnknown, "inspect_outcome_before_retry")
    } else if code == "database_result_too_wide" {
        (Class::Capacity, "reduce_request_or_cleanup")
    } else if code == "database_query_timeout" {
        (Class::Transient, "inspect_error_before_retry")
    } else if matches!(
        code,
        "conflicting_path_filters"
            | "file_symlink_not_allowed"
            | "file_parent_not_directory"
            | "call_id_collision"
            | "malformed_tool_call"
            | "workflow_locked"
            | "workflow_forbidden"
            | "workflow_selected_by_user"
    ) {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "memory_referenced" {
        (Class::Prerequisite, "reduce_request_or_cleanup")
    } else if code == "history_unavailable" {
        (Class::MissingEvidence, "lookup_observed_evidence")
    } else if code == "file_access_error" {
        (Class::Unavailable, "check_file_permissions")
    } else if matches!(
        code,
        "search_too_broad" | "outline_too_broad" | "navigation_too_broad" | "scope_timeout"
    ) {
        (Class::Capacity, "reduce_request_or_cleanup")
    } else if code == "document_write_verification_failed" {
        (Class::OutcomeUnknown, "inspect_outcome_before_retry")
    } else if code.contains("conflict")
        || code.contains("changed")
        || code.contains("stale")
        || code.ends_with("_expired")
        || matches!(
            code,
            "invalid_cursor" | "cursor_expired" | "memory_not_found"
        )
    {
        (Class::StaleState, "refresh_matching_state")
    } else if code.contains("capacity") || code.contains("limit") || code.contains("budget") {
        (Class::Capacity, "reduce_request_or_cleanup")
    } else if matches!(
        code,
        "question_tools_not_allowed"
            | "checkpoint_pending"
            | "tool_not_active"
            | "closing_mode"
            | "gap_requires_closing"
            | "workflow_write_scope"
    ) || code.starts_with("unsupported")
    {
        (Class::Unavailable, "use_available_tools")
    } else if code.contains("timeout") {
        (Class::Transient, "inspect_timeout_before_retry")
    } else if code.starts_with("invalid_")
        || code.starts_with("missing_")
        || code.starts_with("unknown_argument")
        || code == "unknown_optional_tool_or_basic_tool"
        || code == "ambiguous_file_read_range"
        || code == "conflicting_arguments"
        || code == "item_already_verified"
        || code == "whole_write_withheld"
        || code == "completion_required"
    {
        (Class::InvalidInput, "correct_arguments")
    } else {
        (Class::Unclassified, "inspect_error_before_retry")
    };
    json!({"code":code,"class":class,"action":action,"automatic_retry":false})
}

/// Filter recovery hints against the exact currently offered tool set. A
/// checkpoint must never suggest a file read that its executor will reject.
pub fn attach(s: &Session, call: &crate::llm::ToolCall, result: &mut Value) {
    if result["status"] == "ok" {
        return;
    }
    // Batch items need the same actionable, availability-filtered recovery
    // contract as standalone calls. Preserve successful siblings unchanged.
    if result["recovery"]["code"] == "batch_partial_failure" {
        if !result["data"]["results"].is_array() {
            // The worker may already have archived the detailed items. Keep
            // its derived decision; missing detail is not a new empty batch.
            let available = ToolRegistry::definitions(s);
            if let Some(hints) = result
                .get_mut("recovery")
                .and_then(|recovery| recovery.get_mut("tools"))
                .and_then(Value::as_array_mut)
            {
                hints.retain(|hint| available.iter().any(|d| d["function"]["name"] == *hint));
            }
            return;
        }
        let mut hints = Vec::<Value>::new();
        let mut uncertain = false;
        let mut cancelled = false;
        if let Some(items) = result["data"]["results"].as_array_mut() {
            for item in items {
                let nested = if item.get("result").is_some_and(Value::is_object) {
                    &mut item["result"]
                } else {
                    item
                };
                if is_failure(nested) {
                    attach(s, call, nested);
                    uncertain |= nested["recovery"]["class"] == "outcome_unknown"
                        || nested["recovery"]["action"] == "inspect_outcome_before_retry";
                    cancelled |=
                        nested["status"] == "cancelled" || nested["recovery"]["action"] == "stop";
                    for hint in nested["recovery"]["tools"].as_array().into_iter().flatten() {
                        if !hints.contains(hint) {
                            hints.push(hint.clone());
                        }
                    }
                }
            }
        }
        result["recovery"]["tools"] = json!(hints);
        result["recovery"]["document_repairable"] = json!(correctable_document_error(result));
        if uncertain {
            result["recovery"]["action"] = json!("inspect_outcome_before_retry");
        } else if cancelled {
            result["recovery"]["action"] = json!("stop");
            result["recovery"]["tools"] = json!([]);
        }
        return;
    }
    if result["recovery"].is_null() {
        result["recovery"] = describe(result["error"].as_str().unwrap_or("tool_error"));
    }
    let candidates: &[&str] = match result["recovery"]["action"].as_str().unwrap_or("") {
        "configure_database_in_settings" => &[],
        "select_enabled_database_query" => &["db_query"],
        "correct_arguments" if call.name == "db_query" => &["db_query"],
        "correct_arguments" if call.name == "db_execute" => &["db_execute"],
        "correct_arguments" if call.name == "history" => &["history"],
        "correct_arguments" if call.name == "source_lookup" => &["source_lookup"],
        "correct_arguments" if call.name == "checkpoint_complete" => &["checkpoint_complete"],
        "correct_arguments" if call.name == "tool_catalog" => &["tool_catalog"],
        "restore_memory_evidence" => &["memory_read", "source_lookup", "history"],
        "supply_memory_revision" => &[
            "memory_read",
            "memory_find",
            "memory_write",
            "memory_manage",
        ],
        "repair_memory_references" => &[
            "memory_read",
            "memory_find",
            "source_lookup",
            "file_read",
            "memory_write",
            "task_state",
            "history",
        ],
        "repair_checkpoint_on_next_request" => &["history", "checkpoint_complete"],
        "inspect_checkpoint_state" => &["history", "task_state"],
        "lookup_observed_evidence" => &["source_lookup", "history", "file_read"],
        "complete_prerequisite" => &["document_inspect", "document_edit"],
        "resolve_path" if matches!(call.name.as_str(), "document_edit" | "document_edit_batch") => {
            &["document_inspect", "document_edit", "document_edit_batch"]
        }
        "resolve_path" => &["document_inspect", "file_list"],
        "select_file_from_directory" => &["file_list", "file_read"],
        "choose_allowed_path" => &["file_list", "document_inspect"],
        // The edit was checked in memory; resend it with the hash.
        "copy_document_hash" if call.name == "document_edit" => {
            &["document_inspect", "document_edit"]
        }
        "copy_document_hash" if call.name == "document_edit_batch" => {
            &["document_inspect", "document_edit_batch"]
        }
        "copy_document_hash" => &["document_inspect"],
        "copy_file_hash" => &["file_read"],
        "use_document_editor" => &["document_inspect", "document_edit", "document_edit_batch"],
        // The message carries the current hash; editing is the next step.
        "inspect_document_outline" if result["recovery"]["code"] == "document_exists" => {
            &["document_inspect", "document_edit", "document_edit_batch"]
        }
        "restart_document_inspection" | "inspect_document_outline" => &["document_inspect"],
        "choose_exact_section" => &["document_inspect", "file_read"],
        "repair_document_citation" => &["document_inspect", "document_edit", "document_edit_batch"],
        "copy_observed_symbol_id" => &["code_outline", "symbol_read"],
        "check_file_permissions" => &["file_list", "file_read", "document_inspect"],
        "inspect_timeout_before_retry" => &[
            "history",
            "document_inspect",
            "file_read",
            "source_search",
            "code_outline",
        ],
        "inspect_outcome_before_retry" if call.name == "db_execute" => &["history", "db_query"],
        "use_available_tools" if result["recovery"]["code"] == "checkpoint_pending" => {
            &["checkpoint_complete", "memory_write", "task_state"]
        }
        "inspect_outcome_before_retry" => &["history", "document_inspect", "file_read"],
        "use_available_tools" if result["recovery"]["code"] == "workflow_write_scope" => {
            &["document_inspect", "document_edit", "document_edit_batch"]
        }
        "use_available_tools" if result["recovery"]["code"] == "unsupported_language" => {
            &["source_search", "file_read", "tool_catalog", "tool_select"]
        }
        "correct_arguments" if call.name == "document_inspect" => {
            &["document_inspect", "file_read"]
        }
        "correct_arguments" if call.name == "document_audit" => &["document_audit"],
        "correct_arguments" if call.name == "document_edit" => {
            &["document_inspect", "document_edit"]
        }
        "correct_arguments" if call.name == "document_edit_batch" => {
            &["document_inspect", "document_edit_batch"]
        }
        "correct_arguments" if call.name == "file_read" => &["file_read"],
        "correct_arguments"
            if matches!(
                call.name.as_str(),
                "file_edit" | "file_write" | "file_patch"
            ) =>
        {
            &["file_read", "file_edit", "file_write", "file_patch"]
        }
        "correct_arguments" if call.name == "tool_select" => &["tool_select", "task_state"],
        "correct_arguments" if call.name == "task_state" => &["task_state"],
        "correct_arguments" if call.name == "memory_write" => {
            &["memory_write", "source_lookup", "history"]
        }
        "correct_arguments" if call.name == "memory_manage" => {
            &["memory_manage", "memory_read", "source_lookup", "history"]
        }
        "correct_arguments" if call.name == "task_plan" => &["task_plan"],
        "correct_arguments" if call.name == "file_list" => &["file_list"],
        "correct_arguments" if call.name == "source_search" => &["source_search"],
        "correct_arguments" if call.name == "symbol_search" => &["symbol_search"],
        "correct_arguments" if call.name == "symbol_relations" => {
            &["symbol_relations", "symbol_search", "code_outline"]
        }
        "correct_arguments" if call.name == "code_outline" => &["code_outline"],
        "correct_arguments" if call.name == "symbol_read" => &["symbol_read", "code_outline"],
        // Memory-specific stale errors need the broader recovery set below:
        // the generic memory branch would otherwise match first and make the
        // code-specific branch unreachable.
        "refresh_matching_state"
            if matches!(
                result["recovery"]["code"].as_str(),
                Some("memory_not_found" | "memory_changed")
            ) =>
        {
            &[
                "memory_find",
                "memory_read",
                "memory_manage",
                "task_state",
                "source_lookup",
                "history",
            ]
        }
        "refresh_matching_state" if call.name.starts_with("memory_") => {
            &["memory_read", "memory_find"]
        }
        "refresh_matching_state"
            if matches!(call.name.as_str(), "document_edit" | "document_edit_batch") =>
        {
            &["document_inspect", "document_edit", "document_edit_batch"]
        }
        "refresh_matching_state" if call.name == "document_inspect" => {
            &["document_inspect", "file_read"]
        }
        "refresh_matching_state" if call.name == "document_audit" => {
            &["document_audit", "document_inspect"]
        }
        "refresh_matching_state" if call.name == "source_search" => &["source_search", "file_read"],
        "refresh_matching_state" if call.name == "file_list" => &["file_list"],
        "refresh_matching_state"
            if matches!(
                call.name.as_str(),
                "file_edit" | "file_write" | "file_patch"
            ) =>
        {
            &["file_read", "file_list"]
        }
        "refresh_matching_state" if call.name == "symbol_search" => &["symbol_search", "file_read"],
        "refresh_matching_state" if call.name == "symbol_relations" => {
            &["symbol_search", "code_outline", "symbol_relations"]
        }
        "refresh_matching_state" => &["code_outline", "file_read", "source_lookup", "history"],
        "reduce_request_or_cleanup" if result["recovery"]["code"] == "memory_body_limit" => {
            &["memory_write", "memory_manage"]
        }
        "reduce_request_or_cleanup"
            if matches!(
                result["recovery"]["code"].as_str(),
                Some("task_state_limit" | "task_detail_limit")
            ) =>
        {
            &["task_state", "memory_write"]
        }
        // References live in task_state memory_ids.
        "reduce_request_or_cleanup" if result["recovery"]["code"] == "memory_referenced" => {
            &["task_state", "memory_manage"]
        }
        "reduce_request_or_cleanup" if call.name == "source_search" => &["source_search"],
        "reduce_request_or_cleanup" if call.name == "db_query" => &["db_query"],
        "reduce_request_or_cleanup" if call.name == "db_execute" => &["db_execute", "db_query"],
        "reduce_request_or_cleanup"
            if call.name == "file_list"
                && matches!(
                    result["recovery"]["code"].as_str(),
                    Some("file_scan_capacity" | "scope_timeout")
                ) =>
        {
            &["file_list"]
        }
        "reduce_request_or_cleanup" if call.name == "file_patch" => &["file_patch", "file_read"],
        "reduce_request_or_cleanup" if call.name == "code_outline" => &["code_outline"],
        "reduce_request_or_cleanup"
            if matches!(call.name.as_str(), "symbol_search" | "symbol_relations") =>
        {
            &["symbol_search", "symbol_relations", "file_list"]
        }
        "reduce_request_or_cleanup" => &[
            "memory_manage",
            "task_state",
            "history",
            "checkpoint_complete",
        ],
        "use_available_tools" => &["tool_catalog", "tool_select", "checkpoint_complete"],
        "stop" => &[],
        _ => &["history"],
    };
    let definitions = ToolRegistry::definitions(s);
    let available: Vec<_> = candidates
        .iter()
        .filter(|name| **name != "checkpoint_complete" || s.checkpoint.is_some())
        .filter(|name| definitions.iter().any(|d| d["function"]["name"] == **name))
        .copied()
        .collect();
    let mut available = available;
    // Typed-but-invalid arguments are recoverable by correcting and resending
    // the same call, so that tool leads the list (memory_read and memory_find
    // once got only "history"). Keep it availability-filtered so checkpoints
    // and inactive optional tools never receive an impossible hint.
    // An unknown tool name leads with the offered tool it most likely meant.
    if result["recovery"]["code"] == "unsupported_tool"
        && let Some(meant) = result["data"]["did_you_mean"].as_str()
        && !available.contains(&meant)
        && definitions
            .iter()
            .any(|definition| definition["function"]["name"] == meant)
    {
        available.insert(0, meant);
    }
    // A corrected path is resent to the same tool as well.
    if matches!(
        result["recovery"]["action"].as_str(),
        Some("correct_arguments" | "resolve_path")
    ) && !available.contains(&call.name.as_str())
        && definitions
            .iter()
            .any(|definition| definition["function"]["name"] == call.name)
    {
        if result["recovery"]["action"] == "correct_arguments" {
            available.insert(0, call.name.as_str());
        } else {
            available.push(call.name.as_str());
        }
    }
    result["recovery"]["tools"] = json!(available);
}

/// Failures cannot evade the bound by changing an ID, argument or recovery
/// code every round. Identical invalid calls are surfaced immediately so the
/// agent can change approach before the general per-tool budget is exhausted.
/// A success resets only that tool's failures; unrelated writes cannot hide it.
#[derive(Default)]
pub struct FailureTracker {
    by_tool_and_code: VecDeque<CodeFailure>,
    by_tool: BTreeMap<&'static str, usize>,
    by_invocation: VecDeque<InvocationFailure>,
    repeated_calls: VecDeque<RepeatedCall>,
}

/// The same call that failed before for the same reason, across tools.
struct RepeatedCall {
    call: [u8; 32],
    reason: [u8; 32],
    count: usize,
}

struct CodeFailure {
    tool: &'static str,
    code: [u8; 32],
    count: usize,
}

struct InvocationFailure {
    tool: &'static str,
    arguments: [u8; 32],
    code: [u8; 32],
    error: [u8; 32],
    count: usize,
}

/// Every failed item in a mixed batch must be correctable. An uncertain write
/// cannot inherit the recovery policy of ordinary argument/evidence errors.
pub fn correctable_document_error(result: &Value) -> bool {
    match result["recovery"]["class"].as_str() {
        Some(
            "invalid_input" | "prerequisite" | "missing_path" | "missing_evidence" | "stale_state"
            | "capacity",
        ) => true,
        // Tool selection, a text-reader fallback and checkpoint completion are
        // available remedies; these do not mean the runtime itself is lost.
        Some("unavailable") => matches!(
            result["recovery"]["code"].as_str(),
            Some(
                "tool_not_active"
                    | "unsupported_tool"
                    | "unsupported_language"
                    | "checkpoint_pending"
                    | "closing_mode"
                    | "gap_requires_closing"
            )
        ),
        Some("partial_failure") => result["data"]["results"].as_array().map_or_else(
            || {
                result["truncated"] == true
                    && result["recovery"]["document_repairable"] == true
                    && result["recovery"]["action"] == "repair_failed_items_only"
            },
            |items| {
                !items.is_empty()
                    && items.iter().all(|item| {
                        let result = batch_result(item);
                        result["status"] == "ok"
                            || (result["status"] != "cancelled"
                                && correctable_document_error(result))
                    })
            },
        ),
        _ => false,
    }
}

/// Replace the ordinary retry hint after an identical document-edit failure
/// with an explicit instruction to inspect state and change the edit strategy.
pub fn annotate_identical_document_failure(result: &mut Value) {
    result["recovery"]["repeat_detected"] = json!(true);
    result["recovery"]["action"] = json!("change_approach");
    result["recovery"]["tools"] = json!(["document_inspect", "document_edit"]);
    result["recovery"]["guidance"] = json!(
        "This exact request already failed with the same cause. Do not resubmit it. Inspect the current outline and failure cause, then make one targeted document_edit using the current document hash."
    );
}

impl FailureTracker {
    // Correctable document failures can continue after the ordinary retry
    // limit and history pruning. Bound their separate repeat-detection cache;
    // eviction never resets the aggregate per-tool failure budget.
    const MAX_INVOCATIONS: usize = 128;
    const MAX_CODES: usize = 128;
    const UNSUPPORTED: &'static str = "<unsupported_tool>";

    fn tool_key(tool: &str) -> &'static str {
        // Only registered names may allocate an aggregate counter. A model
        // changing an unsupported name must not grow this map or evade its
        // failure budget. Retain the registry's fixed names once per process.
        static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
        NAMES
            .get_or_init(|| {
                ToolRegistry::specs()
                    .into_iter()
                    .map(|spec| spec.name)
                    .collect()
            })
            .iter()
            .copied()
            .find(|name| *name == tool)
            .unwrap_or(Self::UNSUPPORTED)
    }

    fn note_code(&mut self, tool: &'static str, code: [u8; 32]) -> usize {
        let previous = self
            .by_tool_and_code
            .iter()
            .position(|failure| failure.tool == tool && failure.code == code)
            .and_then(|index| self.by_tool_and_code.remove(index));
        let mut recent = previous.unwrap_or(CodeFailure {
            tool,
            code,
            count: 0,
        });
        recent.count = recent.count.saturating_add(1);
        let count = recent.count;
        // Per-code counts are diagnostic. Eviction does not reset the
        // per-tool totals that enforce the retry limit.
        if self.by_tool_and_code.len() == Self::MAX_CODES {
            self.by_tool_and_code.pop_front();
        }
        self.by_tool_and_code.push_back(recent);
        count
    }

    const MAX_REPEATED_CALLS: usize = 32;

    /// Mark an identical call that fails again for the same reason: resending
    /// it unchanged gets the same result. Failures decided by the arguments or
    /// by a state the call cannot change (input, path, evidence, stale state,
    /// prerequisite, capacity, a tool withheld in closing or a checkpoint, and
    /// unapplied plan batches) qualify. Transient, uncertain, cancelled and
    /// unclassified failures, and waits for tool workers, may succeed on a
    /// later retry. The error text is left unchanged so other identical-failure
    /// checks hold.
    pub fn mark_repeated_failure(&mut self, tool: &str, arguments: &str, result: &mut Value) {
        // An unchanged apply is not a failure, but resending it is a no-op.
        let unapplied_plan = tool == "task_plan"
            && (result["data"]["applied"] == false || result["data"]["unchanged"] == true);
        let reason = if unapplied_plan {
            result["data"]["reason"].as_str().or(Some("unchanged"))
        } else if result["status"] != "ok"
            && matches!(
                result["recovery"]["class"].as_str(),
                Some(
                    "invalid_input"
                        | "missing_path"
                        | "missing_evidence"
                        | "stale_state"
                        | "prerequisite"
                        | "unavailable"
                        | "capacity"
                )
            )
            && result["recovery"]["action"] != "wait_for_tool_workers"
        {
            result["error"].as_str()
        } else {
            None
        };
        let call: [u8; 32] = Sha256::digest(format!("{tool}\0{arguments}").as_bytes()).into();
        let Some(reason) = reason else {
            self.repeated_calls.retain(|repeat| repeat.call != call);
            return;
        };
        let reason: [u8; 32] = Sha256::digest(reason.as_bytes()).into();
        let count = match self
            .repeated_calls
            .iter_mut()
            .find(|repeat| repeat.call == call)
        {
            Some(repeat) if repeat.reason == reason => {
                repeat.count = repeat.count.saturating_add(1);
                repeat.count
            }
            Some(repeat) => {
                repeat.reason = reason;
                repeat.count = 1;
                1
            }
            None => {
                if self.repeated_calls.len() == Self::MAX_REPEATED_CALLS {
                    self.repeated_calls.pop_front();
                }
                self.repeated_calls.push_back(RepeatedCall {
                    call,
                    reason,
                    count: 1,
                });
                1
            }
        };
        if count < 2 {
            return;
        }
        let note = json!({"count":count,"guidance":format!(
            "This exact call already got this same result {} time(s); resending it unchanged gets it again. Change what the error or reason names, or use one of the recovery tools instead.",
            count - 1
        )});
        if unapplied_plan {
            result["data"]["repeated_unchanged"] = note;
        } else {
            result["recovery"]["repeated_unchanged"] = note;
        }
    }

    pub fn observe(
        &mut self,
        tool: &str,
        arguments: &str,
        result: &Value,
        limit: usize,
    ) -> Option<String> {
        if result["status"] == "cancelled" {
            return None;
        }
        let tool_key = Self::tool_key(tool);
        if result["status"] == "ok" {
            if tool_key != Self::UNSUPPORTED {
                self.by_tool_and_code
                    .retain(|failure| failure.tool != tool_key);
                self.by_tool.remove(tool_key);
                self.by_invocation
                    .retain(|failure| failure.tool != tool_key);
            }
            return None;
        }
        let code = result["recovery"]["code"].as_str().unwrap_or("tool_error");
        let code_key = Sha256::digest(code.as_bytes()).into();
        let error = result["error"].as_str().unwrap_or("unknown failure");
        if matches!(tool, "document_edit" | "document_edit_batch")
            && result["recovery"]["class"] == "invalid_input"
        {
            let arguments = Sha256::digest(arguments.as_bytes()).into();
            let error = Sha256::digest(error.as_bytes()).into();
            let previous = self
                .by_invocation
                .iter()
                .position(|failure| {
                    failure.tool == tool_key
                        && failure.arguments == arguments
                        && failure.code == code_key
                })
                .and_then(|index| self.by_invocation.remove(index));
            let mut recent = previous.unwrap_or(InvocationFailure {
                tool: tool_key,
                arguments,
                code: code_key,
                error,
                count: 0,
            });
            if recent.error == error {
                recent.count = recent.count.saturating_add(1);
            } else {
                recent.error = error;
                recent.count = 1;
            }
            let repeated = recent.count >= 2;
            if self.by_invocation.len() == Self::MAX_INVOCATIONS {
                self.by_invocation.pop_front();
            }
            self.by_invocation.push_back(recent);
            if repeated {
                return Some(format!(
                    "identical_tool_failure: {tool} repeated the same invalid arguments and identical cause ({code}); inspect the current state and use one targeted edit with corrected arguments instead of resubmitting this call"
                ));
            }
        }
        let code_count = self.note_code(tool_key, code_key);
        let total_count = self.by_tool.entry(tool_key).or_default();
        *total_count = total_count.saturating_add(1);
        (*total_count >= limit).then(|| format!(
            "tool_recovery_limit: {tool} failed {total_count} times (latest code {code}, code count {code_count}); last cause: {error}; state and completed writes retained"
        ))
    }
}
