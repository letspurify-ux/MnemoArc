use mnemoarc::{
    config::{Config, Project},
    llm::ToolCall,
    session::Session,
    tools::{self, ToolRegistry, recovery::FailureTracker},
};
use serde_json::json;
fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        },
    );
    s.active_tools = ToolRegistry::optional_names();
    s
}
#[test]
fn every_registered_tool_uses_common_failure_contract_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    for spec in ToolRegistry::specs() {
        let before = json!({"task":s.task,"memories":s.memory.entries,"generation":s.memory.generation,"sources":s.sources,"ledger":s.ledger,"history":s.history.bundles,"active":s.active_tools,"pending":s.pending_tools,"investigations":s.investigations});
        let call = ToolCall {
            id: format!("bad-{}", spec.name),
            name: spec.name.into(),
            arguments: "[]".into(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_ne!(result["status"], "ok", "{}", spec.name);
        assert_eq!(result["recovery"]["class"], "invalid_input", "{result}");
        assert_eq!(result["recovery"]["action"], "correct_arguments");
        assert_eq!(result["recovery"]["automatic_retry"], false);
        assert_eq!(
            json!({"task":s.task,"memories":s.memory.entries,"generation":s.memory.generation,"sources":s.sources,"ledger":s.ledger,"history":s.history.bundles,"active":s.active_tools,"pending":s.pending_tools,"investigations":s.investigations}),
            before,
            "{} mutated before validation",
            spec.name
        );
    }
}
#[test]
fn unavailable_recovery_tools_are_never_suggested_in_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("Preserve original".into());
    mnemoarc::context::ContextManager::prepare(&mut s, 60000).unwrap();
    let call = ToolCall {id:"bad-source".into(),name:"memory_write".into(),arguments:json!({"title":"Bad", "summary":"Bad", "body":"Bad", "kind":"fact", "source_ids":["not-observed"]}).to_string()};
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["recovery"]["code"], "unknown_source");
    assert_eq!(
        result["recovery"]["tools"],
        json!(["source_lookup", "history"])
    );
    assert_eq!(s.memory.entries.len(), 0);
    assert!(!s.checkpoint.as_ref().unwrap().acknowledged);
}
#[test]
fn batch_partial_failures_are_visible_without_discarding_successes() {
    let result = tools::envelope(Ok(json!({"results":[
        {"id":"saved", "result":{"status":"ok","data":{"verified":"saved"}}},
        {"id":"bad", "result":{"status":"error","error":"unknown_source: missing"}}
    ]})));
    assert_eq!(result["status"], "error");
    assert_eq!(result["partial_success"], true);
    assert_eq!(result["recovery"]["action"], "repair_failed_items_only");
    assert_eq!(
        result["data"]["results"][0]["result"]["data"]["verified"],
        "saved"
    );
}
#[test]
fn failure_budget_is_per_tool_and_error_not_arguments_or_unrelated_success() {
    let mut failures = FailureTracker::default();
    for i in 0..3 {
        let error = tools::envelope(Err(anyhow::anyhow!("unknown_source: invented-{i}")));
        assert!(failures.observe("memory_write", &error, 3).is_some() == (i == 2));
        assert!(
            failures
                .observe("task_state", &json!({"status":"ok"}), 3)
                .is_none()
        );
    }
    failures.observe("memory_write", &json!({"status":"ok"}), 3);
    assert!(
        failures
            .observe(
                "memory_write",
                &tools::envelope(Err(anyhow::anyhow!("unknown_source: again"))),
                3
            )
            .is_none()
    );
}
#[test]
fn unknown_and_uncertain_errors_never_enable_automatic_replay() {
    for (message, class) in [
        ("Something unexpected happened", "unclassified"),
        (
            "tool_worker_panic: saved before crashing",
            "outcome_unknown",
        ),
        ("cancelled", "cancelled"),
        ("invalid_tool_arguments: broken JSON", "invalid_input"),
    ] {
        let result = tools::envelope(Err(anyhow::anyhow!("{message}")));
        assert_eq!(result["recovery"]["class"], class);
        assert_eq!(result["recovery"]["automatic_retry"], false);
    }
}
#[test]
fn small_results_keep_machine_readable_recovery_and_original_archive() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let call = ToolCall {
        id: "long-error".into(),
        name: "memory_write".into(),
        arguments: "{}".into(),
    };
    let result = tools::envelope(Err(anyhow::anyhow!(
        "unknown_source: missing; {}",
        "details ".repeat(2000)
    )));
    let limited = tools::limit_result(&mut s, &call, result, 200);
    assert_eq!(limited["recovery"]["code"], "unknown_source");
    assert_eq!(limited["recovery"]["action"], "lookup_observed_evidence");
    assert_eq!(limited["next_cursor"]["tool"], "history");
    assert!(
        tools::result_tokens(&call, &limited, &s.config.model) <= 200,
        "{limited}"
    );
    let archive = s
        .history
        .read(limited["next_cursor"]["id"].as_u64().unwrap())
        .unwrap();
    assert!(
        serde_json::to_value(archive)
            .unwrap()
            .to_string()
            .contains("details details")
    );
}

#[test]
fn disabled_memory_reuse_is_respected_by_recovery_hints() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.memory_reuse = false;
    let call = ToolCall {
        id: "conflict".into(),
        name: "memory_write".into(),
        arguments: "{}".into(),
    };
    let mut result = tools::envelope(Err(anyhow::anyhow!("revision_conflict: changed")));
    tools::recovery::attach(&s, &call, &mut result);
    assert_eq!(result["recovery"]["tools"], json!([]));
    assert!(
        ToolRegistry::definitions(&s)
            .iter()
            .all(|d| d["function"]["name"] != "memory_read"
                && d["function"]["name"] != "memory_find")
    );
}

#[test]
fn cleanup_hint_does_not_acknowledge_a_nonexistent_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let s = session(dir.path());
    let call = ToolCall {
        id: "capacity".into(),
        name: "memory_write".into(),
        arguments: "{}".into(),
    };
    let mut result = tools::envelope(Err(anyhow::anyhow!("memory_capacity: full")));
    tools::recovery::attach(&s, &call, &mut result);
    assert!(
        !result["recovery"]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("checkpoint_complete"))
    );
}

#[test]
fn observed_state_and_path_errors_have_actionable_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().parent().unwrap().join("external-summary.md");
    let call = ToolCall {
        id: "path".into(),
        name: "document_inspect".into(),
        arguments: json!({"path":"external-summary.md"}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["recovery"]["code"], "file_not_found");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains(&dir.path().join("external-summary.md").display().to_string())
    );
    for (code, class, action) in [
        (
            "item_must_be_written_before_verification",
            "prerequisite",
            "complete_prerequisite",
        ),
        (
            "cursor_arguments_conflict",
            "invalid_input",
            "correct_arguments",
        ),
        (
            "source_coverage_missing",
            "missing_evidence",
            "lookup_observed_evidence",
        ),
    ] {
        let r = tools::recovery::describe(code);
        assert_eq!(r["class"], class);
        assert_eq!(r["action"], action);
    }
}
