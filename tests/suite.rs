//! One integration test binary for the files that share no process-wide
//! state. Every freshly linked test binary costs a link and, on macOS, a
//! first-run security scan of 10-17 s; 37 separate binaries made the full
//! suite take over 10 minutes. Files stay in place; tests that set
//! environment variables, re-run their own executable, hold the global
//! write gate or are live runs keep their own [[test]] targets in Cargo.toml.
mod support;

#[path = "agent.rs"]
mod agent;
#[path = "backend_sessions.rs"]
mod backend_sessions;
#[path = "closing.rs"]
mod closing;
#[path = "core.rs"]
mod core_tests;
#[path = "database.rs"]
mod database;
#[path = "database_free.rs"]
mod database_free;
#[path = "document_completion_recovery.rs"]
mod document_completion_recovery;
#[path = "document_format.rs"]
mod document_format;
#[path = "document_review.rs"]
mod document_review;
#[path = "document_review_findings.rs"]
mod document_review_findings;
#[path = "documentation.rs"]
mod documentation;
#[path = "evaluation.rs"]
mod evaluation;
#[path = "file_edit.rs"]
mod file_edit;
#[path = "follow_up.rs"]
mod follow_up;
#[path = "memory_recovery.rs"]
mod memory_recovery;
#[path = "memory_retrieval.rs"]
mod memory_retrieval;
#[path = "message_recovery.rs"]
mod message_recovery;
#[path = "navigation.rs"]
mod navigation;
#[path = "navigation_eval.rs"]
mod navigation_eval;
#[path = "progress_recovery.rs"]
mod progress_recovery;
#[path = "run_history.rs"]
mod run_history;
#[path = "search.rs"]
mod search;
#[path = "session_messages.rs"]
mod session_messages;
#[path = "streaming.rs"]
mod streaming;
#[path = "structure.rs"]
mod structure;
#[path = "symbol_precision.rs"]
mod symbol_precision;
#[path = "symbol_regressions.rs"]
mod symbol_regressions;
#[path = "symbol_review_followup.rs"]
mod symbol_review_followup;
#[path = "task_plan.rs"]
mod task_plan;
#[path = "tool_argument_robustness.rs"]
mod tool_argument_robustness;
#[path = "tool_input_diagnostics.rs"]
mod tool_input_diagnostics;
#[path = "tool_recovery.rs"]
mod tool_recovery;
#[path = "web.rs"]
mod web;
#[path = "web_concurrency.rs"]
mod web_concurrency;
#[path = "web_follow_up.rs"]
mod web_follow_up;
#[path = "web_message_recovery.rs"]
mod web_message_recovery;

/// autotests is off, so a new tests/*.rs file runs only when listed here or
/// in Cargo.toml. Fail instead of silently skipping it.
#[test]
fn every_integration_test_file_is_built() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let suite = include_str!("suite.rs");
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "suite.rs" {
            continue;
        }
        assert!(
            suite.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\"")),
            "tests/{name} is in neither tests/suite.rs nor a Cargo.toml [[test]] target"
        );
    }
}
