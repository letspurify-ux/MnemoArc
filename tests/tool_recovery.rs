mod support;
use mnemoarc::{
    config::{Config, Project},
    llm::ToolCall,
    session::Session,
    tools::{self, ToolRegistry, recovery::FailureTracker},
};
use serde_json::json;

#[test]
fn uncertain_nested_write_is_not_treated_as_a_correctable_document_error() {
    let invalid = tools::envelope(Err(anyhow::anyhow!(
        "document_revision_conflict: stale hash"
    )));
    let uncertain = tools::envelope(Err(anyhow::anyhow!(
        "database_commit_uncertain: inspect outcome"
    )));
    let mut mixed = json!({"status":"error","recovery":{"class":"partial_failure"},"data":{"results":[
        {"result":{"status":"ok"}}, {"result":invalid}
    ]}});
    assert!(tools::recovery::correctable_document_error(&mixed));
    mixed["data"]["results"]
        .as_array_mut()
        .unwrap()
        .push(json!({"result":uncertain}));
    assert!(!tools::recovery::correctable_document_error(&mixed));
}

#[test]
fn failed_database_rollback_requires_outcome_inspection() {
    let result = tools::envelope(Err(anyhow::anyhow!(
        "database_rollback_uncertain: operation failed; rollback failed"
    )));
    assert_eq!(result["recovery"]["class"], "outcome_unknown");
    assert_eq!(result["recovery"]["action"], "inspect_outcome_before_retry");
}

fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.active_tools = ToolRegistry::optional_names();
    s
}

#[test]
fn file_scan_capacity_suggests_narrowing_the_available_scan_tool() {
    let dir = tempfile::tempdir().unwrap();
    let s = session(dir.path());
    for name in [
        "file_list",
        "source_search",
        "symbol_search",
        "symbol_relations",
    ] {
        let call = ToolCall {
            id: "scan".into(),
            name: name.into(),
            arguments: "{}".into(),
        };
        let mut result = tools::envelope(Err(anyhow::anyhow!(
            "file_scan_capacity: narrow path or path_glob"
        )));
        tools::recovery::attach(&s, &call, &mut result);
        assert_eq!(result["recovery"]["class"], "capacity");
        assert_eq!(result["recovery"]["action"], "reduce_request_or_cleanup");
        assert!(
            result["recovery"]["tools"]
                .as_array()
                .unwrap()
                .contains(&json!(name))
        );
        assert_eq!(result["recovery"]["automatic_retry"], false);
    }
    let call = ToolCall {
        id: "archive".into(),
        name: "file_list".into(),
        arguments: "{}".into(),
    };
    let mut result = tools::envelope(Err(anyhow::anyhow!("history_capacity: full")));
    tools::recovery::attach(&s, &call, &mut result);
    assert_eq!(
        result["recovery"]["tools"],
        json!(["memory_manage", "task_state", "history"])
    );
}

#[test]
fn source_document_write_scope_precedes_argument_errors_and_points_to_document_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();

    for (name, arguments) in [
        (
            "file_write",
            json!({"path":"manual.md","text":"# Wrong field"}),
        ),
        (
            "file_edit",
            json!({"path":"manual.md","old_text":"before","text":"after"}),
        ),
        (
            "file_patch",
            json!({"operations":{"item":{"action":"add","path":"manual.md","content":"# Wrong shape"}}}),
        ),
    ] {
        let error = ToolRegistry::validate(&s, name, &arguments)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("workflow_write_scope:"),
            "{name}: {error}"
        );

        let call = ToolCall {
            id: format!("withheld-{name}"),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_eq!(
            result["recovery"]["code"], "workflow_write_scope",
            "{name}: {result}"
        );
        assert_eq!(result["recovery"]["class"], "unavailable");
        assert_eq!(result["recovery"]["action"], "use_available_tools");
        assert_eq!(
            result["recovery"]["tools"],
            json!(["document_inspect", "document_edit", "document_edit_batch"])
        );
    }
    assert!(!dir.path().join("manual.md").exists());
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
        assert!(failures.observe("memory_write", "{}", &error, 3).is_some() == (i == 2));
        assert!(
            failures
                .observe("task_state", "{}", &json!({"status":"ok"}), 3)
                .is_none()
        );
    }
    failures.observe("memory_write", "{}", &json!({"status":"ok"}), 3);
    assert!(
        failures
            .observe(
                "memory_write",
                "{}",
                &tools::envelope(Err(anyhow::anyhow!("unknown_source: again"))),
                3
            )
            .is_none()
    );
}

#[test]
fn changing_unsupported_tool_names_cannot_restart_the_failure_budget() {
    let mut failures = FailureTracker::default();
    let mut s = Session::new(Project::default(), support::compact_config());
    for index in 0..3 {
        let call = ToolCall {
            id: format!("unknown-{index}"),
            name: format!("unregistered_tool_{index}"),
            arguments: "{}".into(),
        };
        let error = tools::run_call(&mut s, &call);
        assert_eq!(error["recovery"]["code"], "unsupported_tool");
        let stopped = failures.observe(&call.name, &call.arguments, &error, 3);
        assert_eq!(stopped.is_some(), index == 2);
        // Success by a real tool cannot reset these unsupported calls.
        let _ = failures.observe("task_state", "{}", &json!({"status":"ok"}), 3);
    }
}

#[test]
fn changing_error_codes_and_eviction_do_not_reset_a_registered_tools_failure_budget() {
    fn error(index: usize) -> serde_json::Value {
        let code = format!(
            "invalid_argument_{}{}",
            char::from(b'a' + ((index / 26) % 26) as u8),
            char::from(b'a' + (index % 26) as u8),
        );
        tools::envelope(Err(anyhow::anyhow!("{code}: rejected")))
    }

    let mut failures = FailureTracker::default();
    for index in 0..512 {
        let stopped = failures.observe("file_read", "{}", &error(index), 3);
        assert_eq!(stopped.is_some(), index >= 2);
    }
    let _ = failures.observe("task_state", "{}", &json!({"status":"ok"}), 3);
    let stopped = failures.observe("file_read", "{}", &error(0), 3).unwrap();
    assert!(stopped.contains("file_read failed 513 times"));
    assert!(stopped.contains("invalid_argument_aa"));
    let _ = failures.observe("file_read", "{}", &json!({"status":"ok"}), 3);
    assert!(failures.observe("file_read", "{}", &error(0), 3).is_none());
}

#[test]
fn identical_invalid_document_call_is_reported_on_second_failure() {
    let mut failures = FailureTracker::default();
    let arguments = json!({
        "expected_hash":"abc123",
        "edits":[{"action":"replace_section","section":"## 2","content":"revised"}]
    })
    .to_string();
    let error = tools::envelope(Err(anyhow::anyhow!(
        "document_batch_operation_failed: index=0; cause=ambiguous_section: ## 2"
    )));

    assert!(
        failures
            .observe("document_edit_batch", &arguments, &error, 16)
            .is_none()
    );
    let repeated = failures
        .observe("document_edit_batch", &arguments, &error, 16)
        .unwrap();
    assert!(repeated.starts_with("identical_tool_failure:"));

    let mut corrected = error.clone();
    tools::recovery::annotate_identical_document_failure(&mut corrected);
    assert_eq!(corrected["recovery"]["repeat_detected"], true);
    assert_eq!(corrected["recovery"]["action"], "change_approach");
    assert_eq!(
        corrected["recovery"]["tools"],
        json!(["document_inspect", "document_edit"])
    );
    assert!(
        corrected["recovery"]["guidance"]
            .as_str()
            .unwrap()
            .contains("Do not resubmit it")
    );
}
#[test]
fn recent_document_failures_refresh_without_resetting_the_aggregate_budget() {
    for tool in ["document_edit", "document_edit_batch"] {
        let mut failures = FailureTracker::default();
        let error = tools::envelope(Err(anyhow::anyhow!("section_not_found: missing")));
        let _ = failures.observe(tool, "frequent", &error, 8);
        for index in 0..512 {
            let _ = failures.observe(tool, &format!("heading-{index}"), &error, 8);
            if index % 16 == 0 {
                assert!(
                    failures
                        .observe(tool, "frequent", &error, 8)
                        .unwrap()
                        .starts_with("identical_tool_failure:")
                );
            }
        }
        // Evicting an old invocation must not let argument changes evade the
        // cumulative per-tool budget. A recent repeated failure still wins.
        assert!(
            failures
                .observe(tool, "heading-0", &error, 8)
                .unwrap()
                .starts_with("tool_recovery_limit:")
        );
        assert!(
            failures
                .observe(tool, "heading-511", &error, 8)
                .unwrap()
                .starts_with("identical_tool_failure:")
        );
        let _ = failures.observe("task_state", "{}", &json!({"status":"ok"}), 8);
        assert!(failures.observe(tool, "new heading", &error, 8).is_some());
        let _ = failures.observe(tool, "{}", &json!({"status":"ok"}), 8);
        assert!(failures.observe(tool, "heading-511", &error, 8).is_none());
    }
}

#[test]
fn document_failure_fingerprints_distinguish_changes_at_the_end_of_long_errors() {
    let mut failures = FailureTracker::default();
    let prefix = "section_not_found: ".to_owned() + &"same details ".repeat(2048);
    let first = tools::envelope(Err(anyhow::anyhow!("{prefix}first heading")));
    let changed = tools::envelope(Err(anyhow::anyhow!("{prefix}second heading")));
    assert!(failures.observe("document_edit", "{}", &first, 8).is_none());
    assert!(
        failures
            .observe("document_edit", "{}", &changed, 8)
            .is_none()
    );
    assert!(
        failures
            .observe("document_edit", "{}", &changed, 8)
            .unwrap()
            .starts_with("identical_tool_failure:")
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
    // The bare output file name resolves to the output, not yet written.
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("configured output does not exist yet"),
        "{result}"
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

#[test]
fn missing_document_edit_hash_points_to_document_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("out.md");
    std::fs::write(&s.project.output, "# Existing\n").unwrap();
    let call = ToolCall {
        id: "missing-document-hash".into(),
        name: "document_edit".into(),
        arguments: json!({
            "action": "insert_after",
            "section": "# Existing",
            "text": "# New\n"
        })
        .to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["recovery"]["code"], "document_hash_required");
    assert_eq!(
        result["recovery"]["tools"],
        json!(["document_inspect", "document_edit"])
    );
}

#[test]
fn directory_read_identifies_path_and_available_navigation_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let call = ToolCall {
        id: "directory".into(),
        name: "file_read".into(),
        arguments: json!({"path":"."}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "error");
    assert_eq!(result["recovery"]["code"], "path_is_directory");
    assert_eq!(result["recovery"]["class"], "invalid_input");
    assert_eq!(result["recovery"]["action"], "select_file_from_directory");
    assert_eq!(
        result["recovery"]["tools"],
        json!(["file_list", "file_read"])
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains(&dir.path().canonicalize().unwrap().display().to_string())
    );
}

#[test]
fn paged_document_read_requires_and_accepts_observed_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("summary.md");
    std::fs::write(&s.project.output, "# Summary\nFirst line\nSecond line\n").unwrap();
    let call = |id: &str, arguments: serde_json::Value| ToolCall {
        id: id.into(),
        name: "document_inspect".into(),
        arguments: arguments.to_string(),
    };
    let missing = tools::run_call(
        &mut s,
        &call("missing-hash", json!({"section":"# Summary","offset":1})),
    );
    assert_eq!(missing["recovery"]["code"], "document_hash_required");
    assert_eq!(missing["recovery"]["class"], "invalid_input");
    assert_eq!(missing["recovery"]["action"], "copy_document_hash");
    assert_eq!(missing["recovery"]["tools"], json!(["document_inspect"]));
    let first = tools::run_call(&mut s, &call("first-page", json!({"section":"# Summary"})));
    assert_eq!(first["status"], "ok");
    let resumed = tools::run_call(
        &mut s,
        &call(
            "resumed",
            json!({"section":"# Summary","offset":1,"expected_hash":first["data"]["hash"]}),
        ),
    );
    assert_eq!(resumed["status"], "ok");
    assert_eq!(resumed["data"]["read_offset"], 1);

    std::fs::write(&s.project.output, "# Summary\nChanged\n").unwrap();
    let changed = tools::run_call(
        &mut s,
        &call(
            "changed",
            json!({"section":"# Summary","offset":1,"expected_hash":first["data"]["hash"]}),
        ),
    );
    assert_eq!(changed["recovery"]["code"], "document_revision_conflict");
    assert_eq!(changed["recovery"]["class"], "stale_state");
    assert_eq!(changed["recovery"]["action"], "restart_document_inspection");
    assert_eq!(changed["recovery"]["tools"], json!(["document_inspect"]));
}

#[test]
fn adjacent_document_navigation_errors_have_specific_recovery() {
    for (message, class, action) in [
        (
            "file_cursor_expired: file changed",
            "stale_state",
            "refresh_matching_state",
        ),
        (
            "ambiguous_section: multiple headings",
            "invalid_input",
            "choose_exact_section",
        ),
        (
            "section_not_found: missing heading",
            "invalid_input",
            "inspect_document_outline",
        ),
        (
            "path_outside_project",
            "invalid_input",
            "choose_allowed_path",
        ),
        (
            "file_permission_denied: cannot read",
            "unavailable",
            "check_file_permissions",
        ),
    ] {
        let recovery = tools::recovery::describe(message);
        assert_eq!(recovery["class"], class);
        assert_eq!(recovery["action"], action);
    }
}

#[test]
fn document_offset_error_explains_heading_index_versus_line_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("summary.md");
    std::fs::write(&s.project.output, "# Summary\nFirst line\nSecond line\n").unwrap();
    let first = tools::run_call(
        &mut s,
        &ToolCall {
            id: "outline".into(),
            name: "document_inspect".into(),
            arguments: "{}".into(),
        },
    );
    let inspect = |id: &str, arguments: serde_json::Value| ToolCall {
        id: id.into(),
        name: "document_inspect".into(),
        arguments: arguments.to_string(),
    };
    let outline = tools::run_call(
        &mut s,
        &inspect(
            "wrong-line",
            json!({"offset":244,"expected_hash":first["data"]["hash"]}),
        ),
    );
    assert_eq!(outline["recovery"]["code"], "invalid_offset");
    assert_eq!(
        outline["recovery"]["tools"],
        json!(["document_inspect", "file_read"])
    );
    let explanation = outline["error"].as_str().unwrap();
    assert!(explanation.contains("244") && explanation.contains("1 headings"));
    assert!(explanation.contains("not a document line number"));
    let section = tools::run_call(
        &mut s,
        &inspect(
            "wrong-char",
            json!({"section":"# Summary","offset":244,"expected_hash":first["data"]["hash"]}),
        ),
    );
    assert!(
        section["error"]
            .as_str()
            .unwrap()
            .contains("characters in section")
    );
}

#[test]
fn written_items_require_a_section_without_mutating_existing_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("out.md");
    std::fs::write(&s.project.output, "# Draft\nbody\n").unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"draft","title":"Draft","status":"in_progress"}),
    )
    .unwrap();
    let before = serde_json::to_value(&s.investigations).unwrap();
    for section in [json!(null), json!(""), json!("  ")] {
        let mut args = json!({"action":"upsert","id":"draft","status":"written"});
        if !section.is_null() {
            args["section"] = section;
        }
        let result = tools::run_call(
            &mut s,
            &ToolCall {
                id: "invalid-written".into(),
                name: "investigation".into(),
                arguments: args.to_string(),
            },
        );
        assert_eq!(result["recovery"]["code"], "investigation_section_required");
        assert_eq!(result["recovery"]["tools"], json!(["document_inspect"]));
        assert_eq!(serde_json::to_value(&s.investigations).unwrap(), before);
    }
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"draft","section":"# Draft","status":"written"}),
    )
    .unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"draft","title":"Updated"}),
    )
    .unwrap();
    assert_eq!(s.investigations[0].section, "# Draft");
    assert_eq!(s.investigations[0].status, "written");
}

#[test]
fn failed_batch_items_expose_recovery_tools_and_preserve_successful_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("out.md");
    std::fs::write(&s.project.output, "# Done\na.rs:1\n").unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"a.rs"})).unwrap();
    tools::execute(&mut s, "investigation", json!({"action":"upsert","id":"ready","title":"Ready","status":"written","section":"# Done"})).unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"draft","title":"Draft","status":"in_progress"}),
    )
    .unwrap();
    let entry = json!({"source_ids":[read["source"]["id"]],"verification_note":"Compared source"});
    let result = tools::run_call(
        &mut s,
        &ToolCall {
            id: "mixed".into(),
            name: "investigation".into(),
            arguments: json!({"action":"verify_batch","items":{"draft":entry,"ready":entry}})
                .to_string(),
        },
    );
    assert_eq!(result["recovery"]["code"], "batch_partial_failure");
    assert_eq!(
        result["recovery"]["tools"],
        json!(["document_inspect", "investigation", "document_edit"])
    );
    assert_eq!(result["data"]["summary"]["succeeded"], 1);
    assert_eq!(result["data"]["summary"]["failed"], 1);
    assert_eq!(result["data"]["retry_ids"], json!(["draft"]));
    assert_eq!(result["data"]["succeeded_ids"], json!(["ready"]));
    assert_eq!(
        result["data"]["summary"]["failures_by_code"],
        json!([{"code":"item_must_be_written_before_verification","count":1,"ids":["draft"]}])
    );
    let items = result["data"]["results"].as_array().unwrap();
    let failed = &items.iter().find(|i| i["id"] == "draft").unwrap()["result"];
    assert_eq!(failed["recovery"]["tools"], result["recovery"]["tools"]);
    let success = &items.iter().find(|i| i["id"] == "ready").unwrap()["result"];
    assert_eq!(success["status"], "ok");
    assert!(success["recovery"].is_null());
    assert_eq!(
        s.investigations
            .iter()
            .find(|i| i.id == "ready")
            .unwrap()
            .status,
        "verified"
    );
}

#[test]
fn tool_contract_exposes_action_fields_and_points_state_fields_to_patch() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let definition = ToolRegistry::definitions(&s)
        .into_iter()
        .find(|d| d["function"]["name"] == "investigation")
        .unwrap();
    // Per-action unions are not offered to the model (a provider dropped
    // arguments under them); execution enforces each action's fields.
    assert!(definition["function"]["parameters"].get("oneOf").is_none());
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"x","items":{},"source_ids":[],"verification_note":"n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("does not accept items"), "{error}");
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","source_ids":["S1"],"items":{"x":{"source_ids":[],"verification_note":"n"}}}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("does not accept source_ids"), "{error}");
    let before = s.task.phase.clone();
    let err = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","phase":"verify"}),
    )
    .unwrap_err();
    assert!(err.to_string().contains("belongs inside task_state patch"));
    assert_eq!(s.task.phase, before);
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"phase":"verify"}}),
    )
    .unwrap();
    assert_eq!(s.task.phase, "verify");
    for (code, action) in [
        ("memory_sources_required", "restore_memory_evidence"),
        (
            "checkpoint_has_failed_operations",
            "repair_checkpoint_on_next_request",
        ),
    ] {
        let recovery = tools::recovery::describe(&format!("{code}: test"));
        assert_ne!(recovery["class"], "unclassified");
        assert_eq!(recovery["action"], action);
        assert_eq!(recovery["automatic_retry"], false);
    }
}
