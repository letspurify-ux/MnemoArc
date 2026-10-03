use mnemoarc::{
    config::{Config, Project},
    context::ContextManager,
    llm::ToolCall,
    memory::MemoryStatus,
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            output: root.join("out.md"),
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

fn run(s: &mut Session, id: &str, name: &str, args: Value) -> Value {
    tools::run_call(
        s,
        &ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        },
    )
}

fn fixture(root: &std::path::Path) -> (Session, Value, Value, String) {
    std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.join("out.md"),
        "# Main\nEntry point: a.rs:1\n\n# Other\nAlso a.rs:1\n",
    )
    .unwrap();
    let mut s = session(root);
    let source =
        tools::execute(&mut s, "file_read", json!({"path":"a.rs"})).unwrap()["source"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
    let input = json!({"key":"main-fact","title":"Entry point","summary":"main is declared","body":"main is an empty entry point","kind":"fact","source_ids":[source]});
    let memory = tools::execute(&mut s, "memory_write", input.clone()).unwrap();
    tools::execute(&mut s, "investigation", json!({"action":"upsert","id":"main","title":"Main","status":"written","section":"# Main","memory_ids":[memory["id"]],"source_ids":[source]})).unwrap();
    (s, input, memory, source)
}

fn verify(source: &str) -> Value {
    json!({"action":"verify","id":"main","source_ids":[source],"verification_note":"Compared the empty main declaration with the documented entry point"})
}

#[test]
fn live_missing_and_parameter_markup_arguments_are_actionable_and_never_saved() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The Oct 1 live run lost title/kind and leaked native parameter markup;
    // item was neither a memory field nor an unambiguous source_ids alias.
    for (i, args, missing, unknown) in [
        (
            0,
            json!({"key":"manual","summary":"summary</summary>\n<parameter name=\"tags\">ui</description>","body":"do not echo this body ".repeat(100),"kind":"fact"}),
            json!(["title"]),
            json!([]),
        ),
        (
            1,
            json!({"title":"Manual","summary":"s</summary><parameter name=\"kind\">fact</description>","body":"b","item":"S73"}),
            json!(["kind"]),
            json!(["item"]),
        ),
        (
            2,
            json!({"title":"Manual","parameter name=\"summary\"":"s","body":"b","item":"ui"}),
            json!(["summary", "kind"]),
            json!(["item", "parameter name=\"summary\""]),
        ),
    ] {
        let id = format!("live-shape-{i}");
        let result = run(&mut s, &id, "memory_write", args);
        assert_eq!(result["status"], "error", "{result}");
        assert_eq!(result["recovery"]["action"], "correct_arguments");
        assert!(
            result["recovery"]["tools"]
                .as_array()
                .unwrap()
                .contains(&json!("memory_write"))
        );
        assert_eq!(result["recovery"]["automatic_retry"], false);
        let input = &result["data"]["input_error"];
        assert_eq!(input["missing_fields"], missing);
        assert_eq!(input["unknown_fields"], unknown);
        assert_eq!(input["native_markup"], true);
        assert!(
            input["received_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("body"))
        );
        let example = &result["data"]["correction"]["arguments"];
        for field in ["title", "summary", "body", "kind"] {
            assert!(example.get(field).is_some());
        }
        assert_eq!(result["data"]["correction"]["example_only"], true);
        assert!(!result.to_string().contains("do not echo this body"));
        assert!(s.memory.entries.is_empty());
        assert!(!s.ledger.contains_key(&id));
    }
    let corrected = run(
        &mut s,
        "live-shape-2",
        "memory_write",
        json!({"title":"Question","summary":"What is the entry point?","body":"Still to be observed","kind":"question"}),
    );
    assert_eq!(corrected["status"], "ok");
    assert_eq!(s.memory.entries.len(), 1);
}

#[test]
fn complete_header_markup_is_rejected_but_literal_body_markup_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let mut input = json!({"title":"Protocol","summary":"s</summary><parameter name=\"tags\">ui</description>","body":"Document <parameter name=\"title\"> and </summary> literally","kind":"procedure"});
    let result = run(&mut s, "header", "memory_write", input.clone());
    assert_eq!(result["recovery"]["code"], "invalid_memory_markup");
    assert!(s.memory.entries.is_empty());
    input["summary"] = json!("Provider protocol syntax");
    let saved = run(&mut s, "header", "memory_write", input.clone());
    assert_eq!(saved["status"], "ok", "{saved}");
    assert_eq!(
        s.memory
            .get(saved["data"]["id"].as_str().unwrap())
            .unwrap()
            .body,
        input["body"].as_str().unwrap()
    );
}

#[test]
fn recovered_arg_key_fields_keep_typed_diagnostics_if_a_required_field_is_still_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let result = run(
        &mut s,
        "markup",
        "memory_write",
        json!({"summary":"s","body":"b","tags</arg_key>[\"ui\"]</arg_value><arg_key>title":"Title"}),
    );
    assert_eq!(result["recovery"]["code"], "missing_argument");
    assert_eq!(
        result["data"]["input_error"]["missing_fields"],
        json!(["kind"])
    );
    assert_eq!(result["data"]["input_error"]["markup_recovered"], true);
    assert_eq!(result["data"]["input_error"]["native_markup"], true);
    assert!(s.memory.entries.is_empty());
}

#[test]
fn replacement_fields_receive_the_same_validation_without_removing_memories() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, _, memory, _) = fixture(dir.path());
    let before = json!({"memory":s.memory.entries,"task":s.task,"items":s.investigations});
    let result = run(
        &mut s,
        "bad-replace",
        "memory_manage",
        json!({"action":"replace","ids":[memory["id"]],"replacement":{"summary":"s","body":"b","item":"S46"}}),
    );
    assert_eq!(result["status"], "error");
    assert_eq!(result["data"]["input_error"]["field"], "replacement");
    assert_eq!(
        result["data"]["input_error"]["missing_fields"],
        json!(["title", "kind"])
    );
    assert_eq!(
        result["data"]["input_error"]["unknown_fields"],
        json!(["item"])
    );
    assert_eq!(
        before,
        json!({"memory":s.memory.entries,"task":s.task,"items":s.investigations})
    );
}

#[test]
fn unknown_null_fields_are_rejected_while_known_replacement_placeholders_remain_compatible() {
    for name in ["memory_write", "memory_manage"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, mut input, memory, source) = fixture(dir.path());
        input["expected_revision"] = json!("1");
        input["tags"] = json!("[\"entry\"]");
        input["source_ids"] = json!(json!([source]).to_string());
        input["metadata"] = Value::Null;
        input["item"] = Value::Null;
        let args = |input: &Value| {
            if name == "memory_manage" {
                json!({"action":"replace","ids":[memory["id"]],"replacement":input})
            } else {
                input.clone()
            }
        };
        let before = json!(s.memory.entries);
        let rejected = run(&mut s, "null-item", name, args(&input));
        assert_eq!(
            rejected["recovery"]["code"], "unknown_argument",
            "{rejected}"
        );
        assert_eq!(
            rejected["data"]["input_error"]["unknown_fields"],
            json!(["item"])
        );
        assert_eq!(json!(s.memory.entries), before);
        input.as_object_mut().unwrap().remove("item");
        let saved = run(&mut s, "null-item", name, args(&input));
        assert_eq!(saved["status"], "ok", "{saved}");
        assert_eq!(s.memory.get("main-fact").unwrap().tags, ["entry"]);
    }
}

#[test]
fn revision_on_a_new_key_asks_to_correct_arguments_not_refresh_state() {
    // Live run: the model copied another memory's revision onto a new key.
    // Nothing is stale, so recovery must point back at memory_write itself.
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, _, _) = fixture(dir.path());
    input["key"] = json!("new-fact");
    input["expected_revision"] = json!(5);
    let before = json!(s.memory.entries);
    let result = run(&mut s, "new-key-revision", "memory_write", input.clone());
    assert_eq!(result["status"], "error", "{result}");
    assert_eq!(result["recovery"]["code"], "memory_revision_unexpected");
    assert_eq!(result["recovery"]["class"], "invalid_input");
    assert_eq!(result["recovery"]["action"], "correct_arguments");
    assert_eq!(result["recovery"]["tools"][0], "memory_write");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("omit expected_revision")
    );
    assert_eq!(json!(s.memory.entries), before);
    input.as_object_mut().unwrap().remove("expected_revision");
    let saved = run(&mut s, "new-key-create", "memory_write", input);
    assert_eq!(saved["status"], "ok", "{saved}");
    assert!(s.memory.get("new-fact").is_ok());
}

#[test]
fn missing_and_stale_revisions_are_distinct_and_atomic_for_writes_and_replacements() {
    for name in ["memory_write", "memory_manage"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, mut input, memory, _) = fixture(dir.path());
        s.task.memory_ids = vec![memory["id"].as_str().unwrap().into()];
        input["body"] = json!("main has no parameters and an empty body");
        let before = json!({"memory":s.memory.entries,"generation":s.memory.generation,"task":s.task,"items":s.investigations});
        for (i, expected, code, action) in [
            (
                0,
                Value::Null,
                "memory_revision_missing",
                "supply_memory_revision",
            ),
            (1, json!(0), "revision_conflict", "refresh_matching_state"),
        ] {
            input["expected_revision"] = expected.clone();
            let args = if name == "memory_manage" {
                json!({"action":"replace","ids":[memory["id"]],"replacement":input})
            } else {
                input.clone()
            };
            let result = run(&mut s, &format!("revision-{i}"), name, args);
            assert_eq!(result["recovery"]["code"], code, "{result}");
            assert_eq!(result["recovery"]["action"], action);
            assert_eq!(result["data"]["memory"]["id"], memory["id"]);
            assert_eq!(result["data"]["memory"]["key"], "main-fact");
            assert_eq!(result["data"]["memory"]["status"], "active");
            assert_eq!(result["data"]["memory"]["expected_revision"], expected);
            assert_eq!(result["data"]["memory"]["current_revision"], 1);
            assert_eq!(
                result["data"]["refresh_call"],
                json!({"tool":"memory_read","arguments":{"id":memory["id"]}})
            );
            let patch = &result["data"]["retry"]["argument_patch"];
            assert_eq!(
                if name == "memory_manage" {
                    &patch["replacement"]["expected_revision"]
                } else {
                    &patch["expected_revision"]
                },
                &json!(1)
            );
            assert_eq!(result["data"]["retry"]["review_required"], true);
            assert_eq!(
                before,
                json!({"memory":s.memory.entries,"generation":s.memory.generation,"task":s.task,"items":s.investigations})
            );
            assert!(!s.ledger.contains_key(&format!("revision-{i}")));
        }
        input["expected_revision"] = json!(1);
        let args = if name == "memory_manage" {
            json!({"action":"replace","ids":[memory["id"]],"replacement":input})
        } else {
            input
        };
        let saved = run(&mut s, "revision-1", name, args);
        assert_eq!(saved["status"], "ok", "{saved}");
        assert_eq!(s.memory.entries.len(), 1);
        let current = s.memory.get("main-fact").unwrap();
        assert_eq!(current.body, "main has no parameters and an empty body");
        if name == "memory_manage" {
            assert_ne!(current.id, memory["id"].as_str().unwrap());
            assert_eq!(
                s.task.memory_ids.as_slice(),
                std::slice::from_ref(&current.id)
            );
            assert_eq!(
                s.investigations[0].memory_refs.get(&current.id),
                Some(&current.revision)
            );
        } else {
            assert_eq!(current.revision, 2);
        }
    }
}

#[test]
fn argument_and_revision_hints_remain_available_inside_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, input, _, _) = fixture(dir.path());
    s.add_user("Continue the manual".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    assert!(s.checkpoint.is_some());
    for (id, args) in [
        (
            "missing-title",
            json!({"body":"b","summary":"s","kind":"fact"}),
        ),
        ("missing-revision", input),
    ] {
        let result = run(&mut s, id, "memory_write", args);
        let hints = result["recovery"]["tools"].as_array().unwrap();
        assert!(hints.contains(&json!("memory_write")), "{result}");
        let offered = ToolRegistry::definitions(&s);
        for hint in hints {
            assert!(offered.iter().any(|d| d["function"]["name"] == *hint));
        }
        assert!(!hints.contains(&json!("file_read")));
        assert!(!s.checkpoint.as_ref().unwrap().acknowledged);
    }
}

#[test]
fn reading_a_changed_memory_does_not_refresh_references_but_explicit_upsert_does() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, memory, source) = fixture(dir.path());
    let other = tools::execute(&mut s, "memory_write", json!({"key":"other","title":"Other fact","summary":"Also main","body":"Same observed empty declaration","kind":"fact","source_ids":[source]})).unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"main","memory_ids":[memory["id"],other["id"]]}),
    )
    .unwrap();
    input["expected_revision"] = json!(1);
    input["body"] = json!("main has no parameters and an empty body");
    tools::execute(&mut s, "memory_write", input).unwrap();
    let result = run(&mut s, "verify-stale", "investigation", verify(&source));
    assert_eq!(
        result["recovery"]["code"], "memory_reference_stale",
        "{result}"
    );
    assert_eq!(result["recovery"]["action"], "repair_memory_references");
    let issue = &result["data"]["memory_issues"][0];
    assert_eq!(issue["id"], memory["id"]);
    assert_eq!(issue["referenced_revision"], 1);
    assert_eq!(issue["current_revision"], 2);
    assert_eq!(issue["status"], "active");
    assert_eq!(issue["cause"], "revision_mismatch");
    assert_eq!(result["data"]["reference_update"]["ready"], true);
    tools::execute(
        &mut s,
        "memory_read",
        issue["read_call"]["arguments"].clone(),
    )
    .unwrap();
    let still_stale = run(
        &mut s,
        "verify-after-read",
        "investigation",
        verify(&source),
    );
    assert_eq!(still_stale["recovery"]["code"], "memory_reference_stale");
    assert_eq!(
        s.investigations[0]
            .memory_refs
            .get(memory["id"].as_str().unwrap()),
        Some(&1)
    );
    let update = &result["data"]["reference_update"]["arguments"];
    assert_eq!(update["memory_ids"].as_array().unwrap().len(), 2);
    tools::execute(&mut s, "investigation", update.clone()).unwrap();
    let verified = run(&mut s, "verify-repaired", "investigation", verify(&source));
    assert_eq!(verified["status"], "ok", "{verified}");
    assert_eq!(s.investigations[0].status, "verified");
    assert_eq!(s.investigations[0].memory_refs.len(), 2);
}

#[test]
fn inferred_progress_cannot_verify_even_after_reference_refresh_and_needs_evidence_repair() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, memory, source) = fixture(dir.path());
    input["expected_revision"] = json!(1);
    input["inferred"] = json!(true);
    input["source_ids"] = json!([]);
    input["body"] = json!("Drafting chapter 5; verification remains");
    tools::execute(&mut s, "memory_write", input.clone()).unwrap();
    let result = run(&mut s, "verify-progress", "investigation", verify(&source));
    assert_eq!(
        result["recovery"]["code"], "memory_reference_unverified",
        "{result}"
    );
    assert_eq!(result["data"]["memory_issues"][0]["cause"], "needs_review");
    assert_eq!(result["data"]["reference_update"]["ready"], false);
    tools::execute(
        &mut s,
        "investigation",
        result["data"]["reference_update"]["arguments"].clone(),
    )
    .unwrap();
    let still_unverified = run(
        &mut s,
        "verify-refreshed-progress",
        "investigation",
        verify(&source),
    );
    assert_eq!(
        still_unverified["recovery"]["code"],
        "memory_reference_unverified"
    );
    assert_eq!(s.investigations[0].memory_refs.len(), 1);
    assert_ne!(s.investigations[0].status, "verified");
    input["expected_revision"] = json!(2);
    input["inferred"] = json!(false);
    input["source_ids"] = json!([source]);
    input["body"] = json!("main is an empty entry point");
    tools::execute(&mut s, "memory_write", input).unwrap();
    let repaired = run(&mut s, "verify-restored", "investigation", verify(&source));
    assert_eq!(repaired["recovery"]["code"], "memory_reference_stale");
    tools::execute(
        &mut s,
        "investigation",
        repaired["data"]["reference_update"]["arguments"].clone(),
    )
    .unwrap();
    assert_eq!(
        run(&mut s, "verify-final", "investigation", verify(&source))["status"],
        "ok"
    );
    assert_eq!(
        s.investigations[0]
            .memory_refs
            .get(memory["id"].as_str().unwrap()),
        Some(&3)
    );
}

#[test]
fn missing_and_superseded_references_have_distinct_details_without_automatic_detachment() {
    for missing in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _, memory, source) = fixture(dir.path());
        let id = memory["id"].as_str().unwrap();
        if missing {
            s.memory.entries.remove(id);
        } else {
            s.memory.entries.get_mut(id).unwrap().status = MemoryStatus::Superseded;
        }
        let result = run(&mut s, "verify-invalid", "investigation", verify(&source));
        assert_eq!(
            result["recovery"]["code"],
            if missing {
                "memory_reference_missing"
            } else {
                "memory_reference_unverified"
            }
        );
        assert_eq!(
            result["data"]["memory_issues"][0]["cause"],
            if missing { "not_found" } else { "superseded" }
        );
        assert_eq!(result["data"]["reference_update"]["ready"], false);
        assert_eq!(s.investigations[0].memory_refs.get(id), Some(&1));
        assert_eq!(s.investigations[0].status, "written");
    }
}

#[test]
fn mixed_reference_failures_report_all_causes_and_preserve_all_references() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, memory, source) = fixture(dir.path());
    let inferred = tools::execute(&mut s, "memory_write", json!({"key":"inferred","title":"Inference","summary":"Unverified","body":"Hypothesis","kind":"fact","inferred":true})).unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"main","memory_ids":[memory["id"],inferred["id"]]}),
    )
    .unwrap();
    s.investigations[0]
        .memory_refs
        .insert("deleted-memory".into(), 9);
    input["expected_revision"] = json!(1);
    input["body"] = json!("main has no parameters and an empty body");
    tools::execute(&mut s, "memory_write", input).unwrap();
    let refs = s.investigations[0].memory_refs.clone();
    let result = run(&mut s, "mixed-refs", "investigation", verify(&source));
    assert_eq!(result["recovery"]["code"], "memory_reference_mixed");
    assert_eq!(result["data"]["memory_issue_count"], 3);
    let issues = result["data"]["memory_issues"].as_array().unwrap();
    for cause in ["revision_mismatch", "needs_review", "not_found"] {
        assert!(issues.iter().any(|issue| issue["cause"] == cause));
    }
    assert_eq!(result["data"]["reference_update"]["ready"], false);
    assert_eq!(
        result["data"]["reference_update"]["arguments"]["memory_ids"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(s.investigations[0].memory_refs, refs);
}

#[test]
fn batch_reference_diagnostics_preserve_successful_siblings_and_identify_only_failed_items() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, memory, source) = fixture(dir.path());
    tools::execute(&mut s, "investigation", json!({"action":"upsert","id":"other","title":"Other","status":"written","section":"# Other","source_ids":[source]})).unwrap();
    input["expected_revision"] = json!(1);
    input["body"] = json!("main has no parameters and an empty body");
    tools::execute(&mut s, "memory_write", input).unwrap();
    let entry = json!({"source_ids":[source],"verification_note":"Compared the declaration and cited statement"});
    let result = run(
        &mut s,
        "batch",
        "investigation",
        json!({"action":"verify_batch","items":{"main":entry,"other":entry}}),
    );
    assert_eq!(result["recovery"]["code"], "batch_partial_failure");
    assert_eq!(result["data"]["retry_ids"], json!(["main"]));
    assert_eq!(result["data"]["succeeded_ids"], json!(["other"]));
    let failed = &result["data"]["results"][0]["result"];
    assert_eq!(failed["recovery"]["code"], "memory_reference_stale");
    assert_eq!(failed["data"]["memory_issues"][0]["id"], memory["id"]);
    assert!(
        failed["recovery"]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("investigation"))
    );
    assert_eq!(
        s.investigations
            .iter()
            .find(|i| i.id == "other")
            .unwrap()
            .status,
        "verified"
    );
    assert_eq!(
        s.investigations
            .iter()
            .find(|i| i.id == "main")
            .unwrap()
            .status,
        "written"
    );
}

#[test]
fn small_memory_results_keep_revision_and_argument_causes_with_full_recovery_in_history() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, input, memory, _) = fixture(dir.path());
    s.config.result_tokens = 200;
    for (id, args) in [
        ("small-revision", input),
        (
            "small-argument",
            json!({"summary":"s","body":"b","kind":"fact","item":"S73"}),
        ),
    ] {
        let call = ToolCall {
            id: id.into(),
            name: "memory_write".into(),
            arguments: args.to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_eq!(result["status"], "error");
        assert!(
            tools::result_tokens(&call, &result, &s.config.model) <= 200,
            "{result}"
        );
        assert_eq!(result["truncated"], true);
        if id == "small-revision" {
            assert_eq!(result["data"]["memory"]["id"], memory["id"], "{result}");
            assert_eq!(result["data"]["memory"]["current_revision"], 1);
            assert!(result["data"]["memory"]["expected_revision"].is_null());
        } else {
            assert_eq!(
                result["data"]["input_error"]["missing_fields"],
                json!(["title"]),
                "{result}"
            );
            assert_eq!(
                result["data"]["input_error"]["unknown_fields"],
                json!(["item"])
            );
        }
        let archive = s
            .history
            .read(result["next_cursor"]["id"].as_u64().unwrap())
            .unwrap();
        let full = &archive.messages[0]["result"];
        assert!(
            full["data"][if id == "small-revision" {
                "retry"
            } else {
                "correction"
            }]
            .is_object()
        );
    }
}

#[test]
fn small_reference_results_preserve_the_item_and_stale_revision_cause() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut input, memory, source) = fixture(dir.path());
    input["expected_revision"] = json!(1);
    input["body"] = json!("main has no parameters and an empty body");
    tools::execute(&mut s, "memory_write", input).unwrap();
    s.config.result_tokens = 200;
    let call = ToolCall {
        id: "small-refs".into(),
        name: "investigation".into(),
        arguments: verify(&source).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["recovery"]["code"], "memory_reference_stale");
    assert_eq!(result["data"]["item_id"], "main", "{result}");
    let issue = &result["data"]["memory_issues"][0];
    assert_eq!(issue["id"], memory["id"]);
    assert_eq!(issue["referenced_revision"], 1);
    assert_eq!(issue["current_revision"], 2);
    assert_eq!(issue["status"], "active");
    assert_eq!(issue["cause"], "revision_mismatch");
    assert!(tools::result_tokens(&call, &result, &s.config.model) <= 200);
    let archive = s
        .history
        .read(result["next_cursor"]["id"].as_u64().unwrap())
        .unwrap();
    assert_eq!(
        archive.messages[0]["result"]["data"]["reference_update"]["ready"],
        true
    );
}

#[test]
fn a_path_source_id_resolves_to_its_delivered_evidence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/a.rs"),
        "fn main() {}\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/unread.rs"), "fn other() {}\n").unwrap();
    let mut s = session(dir.path());
    let read = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"src/a.rs","start_line":1,"max_lines":1}),
    )
    .unwrap();
    let source = read["source"]["id"].as_str().unwrap().to_owned();
    let write = |s: &mut Session, key: &str, ids: Value| {
        run(
            s,
            key,
            "memory_write",
            json!({"key":key,"title":"Entry","summary":"main is declared","body":"main is empty","kind":"fact","source_ids":ids}),
        )
    };
    // The live shape: a project path, or a document citation, in place of an S-ID.
    for (key, path) in [("by-path", "src/a.rs"), ("by-citation", "src/a.rs:1")] {
        let result = write(&mut s, key, json!([path]));
        assert_eq!(result["status"], "ok", "{result}");
        assert_eq!(result["data"]["resolved_source_ids"][path], json!([source]));
        let memory = s.memory.get(key).unwrap();
        assert_eq!(memory.sources.len(), 1);
        assert_eq!(memory.sources[0].id, source);
    }
    // A citation range or a file with no delivered lines is still refused,
    // so a memory never cites evidence the session did not observe.
    for (key, path) in [
        ("unread-range", "src/a.rs:2"),
        ("unread-file", "src/unread.rs"),
    ] {
        let result = write(&mut s, key, json!([path]));
        assert_eq!(result["status"], "error", "{result}");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("no complete lines"),
            "{result}"
        );
    }
}
