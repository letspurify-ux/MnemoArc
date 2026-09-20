//! Shared recovery contract. Classification guides the model; it never retries
//! writes, guesses source IDs, or relaxes validation on the model's behalf.
use super::*;
use serde::Serialize;
use std::collections::BTreeMap;

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

/// Unknown errors default to inspection, never to an assumed safe retry.
pub fn describe(message: &str) -> Value {
    let prefix = message.split(':').next().unwrap_or("");
    let code = if !prefix.is_empty()
        && prefix.len() <= 80
        && prefix.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        prefix
    } else {
        "tool_error"
    };
    let (class, action) = if code == "cancelled" {
        (Class::Cancelled, "stop")
    } else if matches!(code, "tool_worker_panic" | "tool_batch_aborted") {
        (Class::OutcomeUnknown, "inspect_outcome_before_retry")
    } else if code == "batch_partial_failure" {
        (Class::PartialFailure, "repair_failed_items_only")
    } else if code == "checkpoint_has_failed_operations" {
        (Class::Prerequisite, "repair_checkpoint_on_next_request")
    } else if code == "memory_sources_required" {
        (Class::MissingEvidence, "restore_memory_evidence")
    } else if code == "item_must_be_written_before_verification" {
        (Class::Prerequisite, "complete_prerequisite")
    } else if code == "file_not_found" || code == "document_missing" {
        (Class::MissingPath, "resolve_path")
    } else if code == "file_permission_denied" {
        (Class::Unavailable, "check_file_permissions")
    } else if code == "path_is_directory" {
        (Class::InvalidInput, "select_file_from_directory")
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
    } else if code == "document_revision_conflict" {
        (Class::StaleState, "restart_document_inspection")
    } else if code == "ambiguous_section" {
        (Class::InvalidInput, "choose_exact_section")
    } else if matches!(code, "section_not_found" | "investigation_section_required") {
        (Class::InvalidInput, "inspect_document_outline")
    } else if matches!(
        code,
        "cursor_arguments_conflict" | "conflicting_arguments" | "ambiguous_file_read_range"
    ) {
        (Class::InvalidInput, "correct_arguments")
    } else if code == "unknown_source" || code == "source_coverage_missing" {
        (Class::MissingEvidence, "lookup_observed_evidence")
    } else if code == "unknown_symbol" {
        (Class::InvalidInput, "copy_observed_symbol_id")
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
    } else if matches!(code, "checkpoint_pending" | "tool_not_active")
        || code.starts_with("unsupported")
    {
        (Class::Unavailable, "use_available_tools")
    } else if code.contains("timeout") {
        (Class::Transient, "inspect_timeout_before_retry")
    } else if code.starts_with("invalid_")
        || code.starts_with("missing_")
        || code.starts_with("unknown_argument")
        || code == "ambiguous_file_read_range"
        || code == "conflicting_arguments"
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
        let mut hints = Vec::<Value>::new();
        if let Some(items) = result["data"]["results"].as_array_mut() {
            for item in items {
                let nested = &mut item["result"];
                if nested.is_object() && nested["status"] != "ok" {
                    attach(s, call, nested);
                    for hint in nested["recovery"]["tools"].as_array().into_iter().flatten() {
                        if !hints.contains(hint) {
                            hints.push(hint.clone());
                        }
                    }
                }
            }
        }
        result["recovery"]["tools"] = json!(hints);
        return;
    }
    if result["recovery"].is_null() {
        result["recovery"] = describe(result["error"].as_str().unwrap_or("tool_error"));
    }
    let candidates: &[&str] = match result["recovery"]["action"].as_str().unwrap_or("") {
        "restore_memory_evidence" => &["memory_read", "source_lookup", "history"],
        "repair_checkpoint_on_next_request" => &["history", "checkpoint_complete"],
        "lookup_observed_evidence" => &["source_lookup", "history", "file_read"],
        "complete_prerequisite" => &["document_inspect", "investigation", "document_edit"],
        "resolve_path" => &["document_inspect", "file_list"],
        "select_file_from_directory" => &["file_list", "file_read"],
        "choose_allowed_path" => &["file_list", "document_inspect"],
        "copy_document_hash" => &["document_inspect"],
        "restart_document_inspection" | "inspect_document_outline" => &["document_inspect"],
        "choose_exact_section" => &["document_inspect", "file_read"],
        "copy_observed_symbol_id" => &["code_outline", "symbol_read"],
        "correct_arguments" if call.name == "document_inspect" => {
            &["document_inspect", "file_read"]
        }
        "correct_arguments" if call.name == "document_audit" => &["document_audit"],
        "correct_arguments" if call.name == "file_read" => &["file_read"],
        "refresh_matching_state" if call.name.starts_with("memory_") => {
            &["memory_read", "memory_find"]
        }
        "refresh_matching_state" if call.name == "document_edit" => &["document_inspect"],
        "refresh_matching_state" => &["code_outline", "file_read", "source_lookup", "history"],
        "reduce_request_or_cleanup" => &[
            "memory_manage",
            "task_state",
            "history",
            "checkpoint_complete",
        ],
        "use_available_tools" => &["tool_catalog", "tool_select", "checkpoint_complete"],
        _ => &[],
    };
    let definitions = ToolRegistry::definitions(s);
    let available: Vec<_> = candidates
        .iter()
        .filter(|name| **name != "checkpoint_complete" || s.checkpoint.is_some())
        .filter(|name| definitions.iter().any(|d| d["function"]["name"] == **name))
        .copied()
        .collect();
    result["recovery"]["tools"] = json!(available);
}

/// Failures cannot evade the bound by changing an ID or argument every round.
/// A success resets only that tool's failures; unrelated writes cannot hide it.
#[derive(Default)]
pub struct FailureTracker(BTreeMap<(String, String), usize>);
impl FailureTracker {
    pub fn observe(&mut self, tool: &str, result: &Value, limit: usize) -> Option<String> {
        if result["status"] == "ok" {
            self.0.retain(|(name, _), _| name != tool);
            return None;
        }
        if result["status"] == "cancelled" {
            return None;
        }
        let code = result["recovery"]["code"].as_str().unwrap_or("tool_error");
        let count = self.0.entry((tool.into(), code.into())).or_default();
        *count += 1;
        (*count >= limit).then(|| format!(
            "tool_recovery_limit: {tool} failed {count} times with {code}; last cause: {}; state and completed writes retained",
            result["error"].as_str().unwrap_or("unknown failure")
        ))
    }
}
