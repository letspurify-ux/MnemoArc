use crate::support;
use mnemoarc::{
    database::SavedQuery,
    llm::ToolCall,
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn session() -> Session {
    let mut s = Session::new(Default::default(), support::compact_config());
    s.active_tools = ToolRegistry::optional_names();
    s.config.database.enabled = true;
    s.config.database.raw_query_enabled = true;
    s.config.database.queries.push(SavedQuery {
        id: "sample".into(),
        description: "test query".into(),
        sql: "SELECT 1 FROM dual".into(),
        enabled: true,
        params: vec![],
    });
    s
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: "input-check".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn example(schema: &Value) -> Value {
    if let Some(first) = schema["enum"].as_array().and_then(|values| values.first()) {
        return first.clone();
    }
    match schema["type"].as_str() {
        Some("string") => json!("x"),
        Some("integer") => json!(1),
        Some("boolean") => json!(false),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => Value::Null,
    }
}

#[test]
fn every_registered_parameter_rejects_wrong_types_with_recovery_before_execution() {
    let mut checked = 0;
    for spec in ToolRegistry::specs() {
        let fields = spec.parameters["properties"].as_object().unwrap();
        for (key, field) in fields {
            // metadata accepts every JSON value. Plan operations deliberately
            // use their own atomic, applied=false recovery contract.
            let Some(kind) = field["type"].as_str() else {
                continue;
            };
            if spec.name == "task_plan" && key == "operations" {
                continue;
            }
            let mut args = json!({});
            for required in spec.parameters["required"].as_array().unwrap() {
                let name = required.as_str().unwrap();
                args[name] = example(&fields[name]);
            }
            args[key] = if kind == "boolean" {
                json!([])
            } else {
                json!(false)
            };
            let mut s = session();
            let before = json!({"task":s.task,"sources":s.sources,"memory":s.memory.entries,
                "active":s.active_tools,"pending":s.pending_tools});
            let result = tools::run_call(&mut s, &call(spec.name, args));
            assert_eq!(result["status"], "error", "{}.{key}: {result}", spec.name);
            assert_eq!(
                result["recovery"]["class"], "invalid_input",
                "{}.{key}: {result}",
                spec.name
            );
            assert_eq!(result["recovery"]["automatic_retry"], false);
            assert!(
                result["data"]["input_error"].is_object(),
                "{}.{key}: {result}",
                spec.name
            );
            assert!(s.ledger.is_empty());
            assert_eq!(
                before,
                json!({"task":s.task,"sources":s.sources,"memory":s.memory.entries,
                "active":s.active_tools,"pending":s.pending_tools})
            );
            checked += 1;
        }
    }
    assert!(checked > 130, "only {checked} parameters checked");
}

#[test]
fn nested_arguments_report_the_exact_path_expected_value_and_actual_type() {
    let mut s = session();
    for (name, args, path, expected, received) in [
        (
            "tool_select",
            json!({"action":"add","names":["file_read",7]}),
            "names[1]",
            json!("string"),
            "number",
        ),
        (
            "task_state",
            json!({"action":"update","patch":{"completion":[false]}}),
            "patch.completion[0]",
            json!("string"),
            "boolean",
        ),
        (
            "task_state",
            json!({"action":"update","patch":{"phase":"invalid"}}),
            "patch.phase",
            json!({"enum":["investigate","draft","verify","answer"]}),
            "string",
        ),
        (
            "file_patch",
            json!({"operations":[{"action":"add","path":"new.txt","content":{}}]}),
            "operations[0].content",
            json!("string"),
            "object",
        ),
        (
            "document_edit_batch",
            json!({"expected_hash":"hash","edits":[{"action":"append","text":false}]}),
            "edits[0].text",
            json!("string"),
            "boolean",
        ),
        (
            "db_execute",
            json!({"mode":"query","sql":"SELECT :x FROM dual","params":{"x":[]}}),
            "params.x",
            json!("string or number or boolean or null"),
            "array",
        ),
        (
            "db_execute",
            json!({"mode":"procedure","name":"TEST","args":[{"name":"p","direction":null}]}),
            "args[0].direction",
            json!("string"),
            "null",
        ),
        (
            "db_query",
            json!({"action":"run","id":"sample","params":{"x":true}}),
            "params.x",
            json!("string or number or null"),
            "boolean",
        ),
    ] {
        let result = tools::run_call(&mut s, &call(name, args));
        assert_eq!(result["status"], "error", "{result}");
        assert_eq!(result["data"]["execution"], "not_started", "{result}");
        assert_eq!(result["data"]["input_error"]["field"], path, "{result}");
        assert_eq!(
            result["data"]["input_error"]["expected"], expected,
            "{result}"
        );
        assert_eq!(
            result["data"]["input_error"]["received"], received,
            "{result}"
        );
    }
}

#[test]
fn schema_bounds_report_the_bound_and_do_not_run_the_tool() {
    for (name, args, field, expected) in [
        (
            "source_lookup",
            json!({"offset":-1}),
            "offset",
            json!({"minimum":0}),
        ),
        (
            "file_read",
            json!({"path":"absent.txt","start_line":0}),
            "start_line",
            json!({"minimum":1}),
        ),
        (
            "file_read",
            json!({"path":"absent.txt","max_lines":2001}),
            "max_lines",
            json!({"maximum":2000}),
        ),
        (
            "source_search",
            json!({"query":"x","before":21}),
            "before",
            json!({"maximum":20}),
        ),
        (
            "source_search",
            json!({"queries":[""]}),
            "queries[0]",
            json!({"minLength":1}),
        ),
        (
            "file_patch",
            json!({"operations":[]}),
            "operations",
            json!({"minItems":1}),
        ),
        (
            "db_execute",
            json!({"mode":"procedure","name":"TEST","args":vec![json!({"name":"p"});33]}),
            "args",
            json!({"maxItems":32}),
        ),
    ] {
        let result = tools::run_call(&mut session(), &call(name, args));
        assert_eq!(result["data"]["execution"], "not_started", "{result}");
        assert_eq!(result["data"]["input_error"]["field"], field, "{result}");
        assert_eq!(
            result["data"]["input_error"]["expected"], expected,
            "{result}"
        );
    }
}

#[test]
fn unknown_null_and_empty_fields_are_not_silently_ignored() {
    for spec in ToolRegistry::specs() {
        for value in [Value::Null, json!("")] {
            let mut args = json!({"path_glob_typo":value});
            for required in spec.parameters["required"].as_array().unwrap() {
                let key = required.as_str().unwrap();
                args[key] = example(&spec.parameters["properties"][key]);
            }
            let result = tools::run_call(&mut session(), &call(spec.name, args));
            assert_eq!(result["status"], "error", "{}: {result}", spec.name);
            assert!(
                result["error"].as_str().unwrap().contains("path_glob_typo"),
                "{}: {result}",
                spec.name
            );
        }
    }
    let result = tools::run_call(
        &mut session(),
        &call("history", json!({"action":"search","id":123})),
    );
    assert_eq!(
        result["recovery"]["code"], "invalid_action_arguments",
        "{result}"
    );
}

#[test]
fn malformed_json_and_bounded_errors_keep_the_correction_visible() {
    let mut s = session();
    let mut invocation = call("task_state", json!({}));
    invocation.arguments = "{\"action\":\"update\",".into();
    let result = tools::run_call(&mut s, &invocation);
    assert_eq!(result["data"]["execution"], "not_started");
    assert_eq!(result["data"]["input_error"]["received"], "invalid JSON");
    assert!(result["data"]["input_error"]["column"].as_u64().unwrap() > 0);

    invocation = call(
        "task_state",
        json!({"action":"update","patch":{"completion":[false]}}),
    );
    let mut result = tools::run_call(&mut s, &invocation);
    result["data"]["details"] = json!("large diagnostic ".repeat(1000));
    let limited = tools::limit_result(&mut s, &invocation, result, 200);
    assert_eq!(limited["data"]["execution"], "not_started", "{limited}");
    assert_eq!(
        limited["data"]["input_error"]["field"], "patch.completion[0]",
        "{limited}"
    );
    assert_eq!(limited["data"]["input_error"]["expected"], "string");
    assert!(
        tools::result_tokens(&invocation, &limited, &s.config.model) <= 200,
        "{limited}"
    );
    assert_eq!(limited["next_cursor"]["tool"], "history");
    let archive = s
        .history
        .read(limited["next_cursor"]["id"].as_u64().unwrap())
        .unwrap();
    assert!(
        serde_json::to_string(archive)
            .unwrap()
            .contains("large diagnostic")
    );
}

#[test]
fn repeated_result_limiting_keeps_the_input_diagnosis_and_original_archive() {
    let mut s = session();
    let invocation = call(
        "task_state",
        json!({"action":"update","patch":{"completion":[false]}}),
    );
    let mut result = tools::run_call(&mut s, &invocation);
    result["data"]["details"] = json!("details ".repeat(1000));
    result["error"] = json!(format!(
        "invalid_argument_type: {}",
        "details ".repeat(1000)
    ));
    let first = tools::limit_result(&mut s, &invocation, result, 600);
    assert!(
        tools::result_tokens(&invocation, &first, &s.config.model) > 200,
        "first pass must still require rebudgeting: {first}"
    );
    let archive = first["next_cursor"]["id"].clone();
    let second = tools::limit_result(&mut s, &invocation, first, 200);
    assert_eq!(
        second["data"]["input_error"]["field"], "patch.completion[0]",
        "{second}"
    );
    assert_eq!(
        second["data"]["input_error"]["expected"], "string",
        "{second}"
    );
    assert_eq!(second["data"]["execution"], "not_started");
    assert_eq!(second["next_cursor"]["id"], archive);
    assert!(tools::result_tokens(&invocation, &second, &s.config.model) <= 200);
}

#[test]
fn archived_batches_keep_the_recovery_decision_after_reattachment() {
    let mut s = session();
    let invocation = call(
        "document_edit_batch",
        json!({"expected_hash":"h","edits":[]}),
    );
    for (code, action, correctable) in [
        ("unknown_source", "repair_failed_items_only", true),
        (
            "database_commit_uncertain",
            "inspect_outcome_before_retry",
            false,
        ),
        ("cancelled", "stop", false),
    ] {
        let mut result = tools::envelope(Ok(json!({"results":[
            {"id":"done","result":{"status":"ok"}},
            {"id":"failed","result":tools::envelope(Err(anyhow::anyhow!("{code}: {}", "details ".repeat(1000))))}
        ]})));
        tools::recovery::attach(&s, &invocation, &mut result);
        let first = tools::limit_result(&mut s, &invocation, result, 250);
        let archive = first["next_cursor"]["id"].clone();
        let mut second = tools::limit_result(&mut s, &invocation, first, 200);
        tools::recovery::attach(&s, &invocation, &mut second);
        assert_eq!(second["recovery"]["action"], action, "{second}");
        assert_eq!(
            tools::recovery::correctable_document_error(&second),
            correctable,
            "{second}"
        );
        assert_eq!(second["partial_success"], true);
        assert_eq!(second["next_cursor"]["id"], archive);
        assert!(
            tools::result_tokens(&invocation, &second, &s.config.model) <= 200,
            "{second}"
        );
    }
}

#[test]
fn empty_symbol_query_is_preserved_as_an_exact_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    for name in ["code_outline", "symbol_search"] {
        let result = tools::run_call(
            &mut s,
            &ToolCall {
                id: name.into(),
                name: name.into(),
                arguments: json!({"path":"a.rs","query":"","match":"exact"}).to_string(),
            },
        );
        assert_eq!(result["status"], "ok", "{result}");
        assert!(
            !result.to_string().contains("main"),
            "empty exact filter was lost: {result}"
        );
    }
}

#[test]
fn batch_status_and_recovery_include_flat_and_nested_failures() {
    let s = session();
    let invocation = call(
        "document_edit_batch",
        json!({"expected_hash":"h","edits":[]}),
    );
    for status in ["error", "cancelled", "unsupported"] {
        for nested in [false, true] {
            let failed = json!({"status":status,"error":"unknown_source: missing"});
            let item = if nested {
                json!({"id":"failed","result":failed})
            } else {
                failed
            };
            let mut result = tools::envelope(Ok(json!({"results":[item]})));
            tools::recovery::attach(&s, &invocation, &mut result);
            assert_eq!(result["status"], "error", "{result}");
            assert_eq!(result["partial_success"], false, "{result}");
            if status == "cancelled" {
                assert_eq!(result["recovery"]["action"], "stop");
                assert_eq!(result["recovery"]["tools"], json!([]));
            } else {
                assert!(
                    result["recovery"]["tools"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("source_lookup")),
                    "{result}"
                );
            }
        }
    }
    let mut result = tools::envelope(Ok(json!({"results":[
        {"status":"ok","data":{"saved":true}},
        {"result":tools::envelope(Err(anyhow::anyhow!("database_commit_uncertain: connection lost")))}
    ]})));
    tools::recovery::attach(&s, &invocation, &mut result);
    assert_eq!(result["partial_success"], true);
    assert_eq!(result["recovery"]["action"], "inspect_outcome_before_retry");
    assert!(!tools::recovery::correctable_document_error(&result));
}

#[test]
fn memory_shape_diagnostics_keep_the_actual_nested_field_and_bound() {
    let memory = json!({"title":"T","summary":"S","body":"B","kind":"fact"});
    for (name, args, field, expected) in [
        (
            "memory_manage",
            json!({"action":"replace","ids":[false],"replacement":memory}),
            "ids[0]",
            json!("string"),
        ),
        (
            "memory_write",
            json!({"title":"T","summary":"S","body":"B","kind":"fact","expected_revision":-1}),
            "expected_revision",
            json!({"minimum":0}),
        ),
    ] {
        let mut s = session();
        let invocation = call(name, args);
        let result = tools::run_call(&mut s, &invocation);
        assert_eq!(result["status"], "error");
        assert_eq!(
            result["data"]["input_error"]["invalid_fields"][0]["field"], field,
            "{result}"
        );
        assert_eq!(
            result["data"]["input_error"]["invalid_fields"][0]["expected"], expected,
            "{result}"
        );
        let limited = tools::limit_result(&mut s, &invocation, result, 250);
        assert_eq!(
            limited["data"]["input_error"]["invalid_fields"][0]["field"], field,
            "{limited}"
        );
        assert_eq!(
            limited["data"]["input_error"]["invalid_fields"][0]["expected"], expected,
            "{limited}"
        );
        assert!(s.memory.entries.is_empty());
    }
    let mut s = session();
    let result = tools::run_call(
        &mut s,
        &call(
            "memory_write",
            json!({"title":false,"summary":false,"body":"B","kind":"fact"}),
        ),
    );
    let invalid = result["data"]["input_error"]["invalid_fields"]
        .as_array()
        .unwrap();
    for field in ["title", "summary"] {
        assert!(
            invalid.iter().any(|entry| entry["field"] == field),
            "{result}"
        );
    }
    assert!(s.memory.entries.is_empty());
}

#[test]
fn action_and_database_bind_errors_name_the_actual_rejected_input() {
    let mut s = session();
    for action in [
        "replace_text",
        "delete_text",
        "insert_before_text",
        "insert_after_text",
    ] {
        let mut edit = json!({"action":action,"old_text":"old","expected_section_hash":"hash"});
        if action != "delete_text" {
            edit["text"] = json!("new");
        }
        let result = tools::run_call(
            &mut s,
            &call(
                "document_edit_batch",
                json!({"expected_hash":"hash","edits":[edit]}),
            ),
        );
        assert_eq!(result["status"], "error", "{result}");
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("edits[0].expected_section_hash"), "{error}");
        assert!(error.contains(&format!("action={action}")), "{error}");
    }
    s.config.database.procedure_enabled = true;
    let result = tools::run_call(
        &mut s,
        &call(
            "db_execute",
            json!({"mode":"procedure","name":"TEST","args":[{"name":"p","type":"number","value":"invalid"}]}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("args[0]: number bind value"), "{error}");
    assert_eq!(
        error
            .matches("invalid_database_execution_arguments:")
            .count(),
        1
    );
}

#[test]
fn database_setup_failures_do_not_recommend_document_edits_and_causes_survive() {
    let s = session();
    let invocation = call("db_execute", json!({}));
    let mut result = tools::envelope(Err(anyhow::anyhow!(
        "database_execution_disabled: enable in Settings"
    )));
    tools::recovery::attach(&s, &invocation, &mut result);
    assert_eq!(
        result["recovery"]["action"],
        "configure_database_in_settings"
    );
    assert_eq!(result["recovery"]["tools"], json!([]));
    let result = tools::envelope(Err(
        anyhow::anyhow!("connection refused").context("database connection failed")
    ));
    assert_eq!(
        result["error"],
        "database connection failed: connection refused"
    );
    assert!(result["data"]["execution"].is_null());
}

#[test]
fn action_specific_errors_name_the_field_and_empty_results_explain_themselves() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "fn run() {}\nfn other() { run(); }\n",
    )
    .unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    let run = |s: &mut Session, name: &str, args: Value| tools::run_call(s, &call_id(name, args));

    let result = run(&mut s, "memory_manage", json!({"action":"delete"}));
    assert_eq!(result["data"]["input_error"]["field"], "ids");
    assert_eq!(result["data"]["input_error"]["received"], "missing");
    assert_eq!(result["data"]["input_error"]["expected"]["type"], "array");
    let result = run(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"bogus":1}}),
    );
    assert_eq!(
        result["data"]["input_error"]["field"], "patch.bogus",
        "{result}"
    );
    assert_eq!(result["data"]["input_error"]["received"], "number");
    // A missing output does not relabel a text-less append as a create.
    let result = run(&mut s, "document_edit", json!({"action":"append"}));
    assert!(
        result["error"].as_str().unwrap().contains("action=append"),
        "{result}"
    );

    let result = run(&mut s, "history", json!({"action":"read","id":99999}));
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("history id 99999 does not exist"), "{error}");
    let result = run(&mut s, "source_lookup", json!({"id":"S42"}));
    assert_eq!(result["status"], "ok");
    assert!(
        result["data"]["notice"]
            .as_str()
            .unwrap()
            .contains("Do not invent an ID")
    );
    let result = run(
        &mut s,
        "file_read",
        json!({"path":"main.rs","start_line":50}),
    );
    assert!(
        result["data"]["notice"]
            .as_str()
            .unwrap()
            .contains("start_line 50 is past the end of the file (total_lines 2)"),
        "{result}"
    );
    let result = run(
        &mut s,
        "file_write",
        json!({"path":"main.rs","content":"x"}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("expected_hash is required because main.rs already exists"),
        "{result}"
    );
    let result = run(
        &mut s,
        "file_edit",
        json!({"path":"main.rs","old_text":"zzz","new_text":"y","expected_hash":"abc"}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("expected_hash \"abc\" does not match the current file"),
        "{result}"
    );
    let result = run(
        &mut s,
        "tool_select",
        json!({"action":"remove","names":["task_state"]}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("task_state is a basic tool that is always available"),
        "{result}"
    );
    let result = run(&mut s, "not_a_tool", json!({}));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("not a tool name; nothing was executed")
    );
    let result = run(&mut s, "db_query", json!({"action":"run","id":"nope"}));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("enabled ids: [\"sample\"]"),
        "{result}"
    );
    let result = run(
        &mut s,
        "db_query",
        json!({"action":"run","id":"sample","params":{"x":1}}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("missing: []; not declared: [\"x\"]"),
        "{result}"
    );
}

#[test]
fn empty_arguments_and_reused_call_ids_are_explained() {
    let mut s = session();
    let empty = ToolCall {
        id: "empty".into(),
        name: "history".into(),
        arguments: "".into(),
    };
    let result = tools::run_call(&mut s, &empty);
    assert_eq!(result["data"]["input_error"]["field"], "action", "{result}");
    let first = call_id("source_lookup", json!({}));
    assert_eq!(tools::run_call(&mut s, &first)["status"], "ok");
    let reused = ToolCall {
        name: "tool_catalog".into(),
        ..first
    };
    let result = tools::run_call(&mut s, &reused);
    let error = result["error"].as_str().unwrap();
    assert!(error.starts_with("call_id_collision: call ID"), "{error}");
    assert!(error.contains("different source_lookup call"), "{error}");
    assert!(error.contains("this call was not executed"), "{error}");
}

fn call_id(name: &str, args: Value) -> ToolCall {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    ToolCall {
        id: format!(
            "diagnostic-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
        name: name.into(),
        arguments: args.to_string(),
    }
}

#[test]
fn a_missing_field_beside_an_unaccepted_one_suggests_the_rename() {
    let mut s = session();
    let result = tools::run_call(
        &mut s,
        &call_id(
            "file_patch",
            json!({"operations":[{"op":"update","path":"main.rs"}]}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.starts_with("missing_argument: operations[0].action is required"),
        "{error}"
    );
    assert!(
        error.contains("if op carries this value, send it as action"),
        "{error}"
    );
    assert_eq!(
        result["data"]["input_error"]["unknown_fields"],
        json!(["op"])
    );
    assert_eq!(result["data"]["execution"], "not_started");
}

#[test]
fn runtime_input_errors_name_the_field_and_claim_no_change_only_with_evidence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn run() {}\n").unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    // Read-only tool: a rejection cannot have changed anything.
    let result = tools::run_call(
        &mut s,
        &call_id("symbol_read", json!({"path":"main.rs","symbol_id":"bogus"})),
    );
    assert_eq!(
        result["data"]["input_error"]["field"], "symbol_id",
        "{result}"
    );
    assert_eq!(result["data"]["input_error"]["received"], "string");
    assert_eq!(result["data"]["execution"], "rejected_without_changes");
    // A write tool that states nothing was persisted.
    let result = tools::run_call(
        &mut s,
        &call_id("file_write", json!({"path":"main.rs","content":"x"})),
    );
    assert_eq!(
        result["data"]["input_error"]["field"], "expected_hash",
        "{result}"
    );
    assert_eq!(result["data"]["input_error"]["received"], "missing");
    assert_eq!(result["data"]["input_error"]["expected"]["type"], "string");
    assert_eq!(result["data"]["execution"], "rejected_without_changes");
    // A write tool without that statement gets no execution claim.
    let invocation = call_id("db_execute", json!({"mode":"query"}));
    let result = tools::run_call(&mut s, &invocation);
    assert_eq!(result["data"]["input_error"]["field"], "sql", "{result}");
    assert!(result["data"]["execution"].is_null(), "{result}");
    // The field name first, but not as the subject: no field is guessed.
    let result = tools::run_call(
        &mut s,
        &call_id("source_search", json!({"query":"(","regex":true})),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_search_regex: regex parse error"),
        "{result}"
    );
    assert!(result["data"]["input_error"].is_null(), "{result}");
    s.config.database.function_enabled = true;
    let result = tools::run_call(
        &mut s,
        &call_id("db_execute", json!({"mode":"function","name":"f"})),
    );
    assert_eq!(
        result["data"]["input_error"]["field"], "return_type",
        "{result}"
    );
    // The diagnosis survives repeated budgets; without an execution claim the
    // compact form keeps the tool name as its marker.
    let mut big = tools::run_call(&mut s, &call_id("db_execute", json!({"mode":"query"})));
    big["error"] = json!(format!(
        "invalid_database_execution_arguments: sql {}",
        "x ".repeat(1000)
    ));
    let first = tools::limit_result(&mut s, &invocation, big, 600);
    assert!(
        tools::result_tokens(&invocation, &first, &s.config.model) > 200,
        "{first}"
    );
    let second = tools::limit_result(&mut s, &invocation, first, 200);
    assert_eq!(second["data"]["input_error"]["field"], "sql", "{second}");
    assert_eq!(
        second["data"]["input_error"]["tool"], "db_execute",
        "{second}"
    );
    assert!(second["data"]["execution"].is_null());
    assert!(tools::result_tokens(&invocation, &second, &s.config.model) <= 200);
}

#[test]
fn file_memory_and_document_failures_name_the_cause_and_matching_remedy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn run() {}\n").unwrap();
    std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 0]).unwrap();
    std::fs::write(
        dir.path().join("summary.md"),
        "# Title\n\nBody.\n\n## Part\n\nMore.\n",
    )
    .unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    let run = |s: &mut Session, name: &str, args: Value| tools::run_call(s, &call_id(name, args));
    let tools_of = |result: &Value| result["recovery"]["tools"].clone();

    // A bare code followed by "; operation_index=0" is still that code.
    let result = run(
        &mut s,
        "file_edit",
        json!({"path":"bin.dat","old_text":"a","new_text":"b","expected_hash":"x"}),
    );
    assert_eq!(
        result["recovery"]["code"], "unsupported_binary_file",
        "{result}"
    );
    assert_eq!(result["recovery"]["action"], "choose_allowed_path");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("bin.dat contains NUL bytes")
    );

    let result = run(&mut s, "file_read", json!({"path":"/etc/hosts"}));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("outside project root"),
        "{result}"
    );

    let result = run(&mut s, "file_read", json!({"path":"main.rs"}));
    let source = result["data"]["source"]["id"].clone();
    let body = "b".repeat(s.config.memory_body_bytes + 1);
    let result = run(
        &mut s,
        "memory_write",
        json!({"title":"t","summary":"s","body":body,"kind":"fact","source_ids":[source]}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("bytes but memory_body_bytes allows"),
        "{result}"
    );
    assert_eq!(tools_of(&result), json!(["memory_write", "memory_manage"]));
    let saved = run(
        &mut s,
        "memory_write",
        json!({"title":"t","summary":"s","body":"b","kind":"fact","source_ids":[source]}),
    );
    let id = saved["data"]["id"].as_str().unwrap().to_owned();
    s.task.memory_ids.push(id.clone());
    let result = run(
        &mut s,
        "memory_manage",
        json!({"action":"delete","ids":[id]}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("nothing was deleted"),
        "{result}"
    );
    assert_eq!(tools_of(&result), json!(["task_state", "memory_manage"]));

    let result = run(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"revision":3}}),
    );
    assert_eq!(
        result["data"]["input_error"]["field"], "patch.revision",
        "{result}"
    );

    s.active_tools.remove("file_list");
    let result = run(&mut s, "file_list", json!({}));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains(r#"tool_select {"action":"add","names":["file_list"]}"#),
        "{result}"
    );
    s.active_tools = ToolRegistry::optional_names();

    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# New\n"}),
    );
    let current = tools::hash(&std::fs::read(dir.path().join("summary.md")).unwrap());
    assert!(
        result["error"].as_str().unwrap().contains(&current),
        "{result}"
    );
    assert!(
        tools_of(&result)
            .as_array()
            .unwrap()
            .contains(&json!("document_edit"))
    );

    let result = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":current,"edits":[
            {"action":"replace_text","old_text":"Body.","text":"Changed."},
            {"action":"replace_text","old_text":"Missing","text":"x"},
            {"action":"section","section":"## Part","text":"## Part\n\nNew.\n","expected_section_hash":"bad"}
        ]}),
    );
    assert_eq!(
        result["data"]["execution"], "rejected_without_changes",
        "{result}"
    );
    assert_eq!(
        result["data"]["failed_edits"],
        json!([
            {"index":1,"action":"replace_text","code":"patch_target_must_match_once"},
            {"index":2,"action":"section","code":"section_revision_conflict"}
        ])
    );
    assert_eq!(
        tools::hash(&std::fs::read(dir.path().join("summary.md")).unwrap()),
        current
    );
}

#[test]
fn an_anchor_outside_the_named_section_names_where_it_is() {
    // Live run 2026-10-06: insert_before_text named the previous section and
    // anchored on the next heading; the error only said "not in the section".
    let dir = tempfile::tempdir().unwrap();
    let doc = "# Manual\n\n## Backend\n\nFlow.\n\n## Data\n\nTypes.\n";
    std::fs::write(dir.path().join("summary.md"), doc).unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    let hash = tools::hash(doc.as_bytes());
    for (name, args) in [
        (
            "document_edit",
            json!({"action":"insert_before_text","section":"## Backend","old_text":"## Data","text":"More flow.\n","expected_hash":hash}),
        ),
        (
            "document_edit_batch",
            json!({"expected_hash":hash,"edits":[{"action":"insert_before_text","section":"## Backend","old_text":"## Data","text":"More flow.\n"}]}),
        ),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, args));
        let error = result["error"].as_str().unwrap();
        assert!(
            error.contains(r###"old_text is not inside section "## Backend" (lines 3-6), but occurs in the document at line 7 under "## Data""###),
            "{error}"
        );
        assert!(error.contains("Omit section"), "{error}");
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("summary.md")).unwrap(),
        doc
    );
}

#[test]
fn an_identical_failure_is_marked_as_unchanged_but_transient_ones_are_not() {
    let mut tracker = tools::recovery::FailureTracker::default();
    let failed = || {
        tools::envelope(Err(anyhow::anyhow!(
            "invalid_argument_value: text has 371 characters"
        )))
    };
    let mut first = failed();
    tracker.mark_repeated_failure("file_edit", r#"{"a":1}"#, &mut first);
    assert!(first["recovery"]["repeated_unchanged"].is_null());
    let mut second = failed();
    tracker.mark_repeated_failure("file_edit", r#"{"a":1}"#, &mut second);
    assert_eq!(
        second["recovery"]["repeated_unchanged"]["count"], 2,
        "{second}"
    );
    // The error text stays identical for other repeated-failure checks.
    assert_eq!(second["error"], first["error"]);
    // Different arguments are a different call.
    let mut other = failed();
    tracker.mark_repeated_failure("file_edit", r#"{"a":2}"#, &mut other);
    assert!(other["recovery"]["repeated_unchanged"].is_null());
    // A transient failure may succeed when retried.
    for _ in 0..2 {
        let mut timeout = tools::envelope(Err(anyhow::anyhow!("request_timeout: slow")));
        tracker.mark_repeated_failure("file_read", "{}", &mut timeout);
        assert!(timeout["recovery"]["repeated_unchanged"].is_null());
    }
    // An unapplied plan batch is status ok but still a repeated rejection.
    let plan = || json!({"status":"ok","data":{"applied":false,"reason":"operations[0] failed: Use nonempty text of at most 240 characters; this text has 371"}});
    let args = r#"{"action":"apply"}"#;
    let mut once = plan();
    tracker.mark_repeated_failure("task_plan", args, &mut once);
    let mut twice = plan();
    tracker.mark_repeated_failure("task_plan", args, &mut twice);
    assert_eq!(twice["data"]["repeated_unchanged"]["count"], 2, "{twice}");
    // A success clears the record.
    let mut applied = json!({"status":"ok","data":{"applied":true}});
    tracker.mark_repeated_failure("task_plan", args, &mut applied);
    let mut again = plan();
    tracker.mark_repeated_failure("task_plan", args, &mut again);
    assert!(again["data"]["repeated_unchanged"].is_null());
}

#[test]
fn a_tool_withheld_by_a_checkpoint_names_itself_and_the_next_step() {
    let mut s = session();
    s.checkpoint = Some(mnemoarc::session::Checkpoint {
        id: "cp-1".into(),
        bundle_ids: vec![],
        maintenance_bundle_ids: vec![],
        acknowledged: false,
        attempts: 0,
        failed_attempts: 0,
        last_failure: None,
        source_lookup_calls: 0,
        starting_state_revision: s.task.revision,
        starting_memory_generation: s.memory.generation,
        failed: false,
    });
    let result = tools::run_call(&mut s, &call_id("file_list", json!({})));
    let error = result["error"].as_str().unwrap();
    assert!(
        error.starts_with(
            "checkpoint_pending: file_list is withheld while checkpoint cp-1 is pending"
        ),
        "{error}"
    );
    assert!(error.contains("Allowed now: memory_write"), "{error}");
    assert_eq!(
        result["recovery"]["tools"][0], "checkpoint_complete",
        "{result}"
    );
}

#[test]
fn a_repeated_unchanged_plan_apply_is_marked() {
    let mut tracker = tools::recovery::FailureTracker::default();
    let args = r#"{"action":"apply","expected_revision":2}"#;
    let unchanged = || json!({"status":"ok","data":{"applied":true,"unchanged":true}});
    let mut first = unchanged();
    tracker.mark_repeated_failure("task_plan", args, &mut first);
    assert!(first["data"]["repeated_unchanged"].is_null());
    let mut second = unchanged();
    tracker.mark_repeated_failure("task_plan", args, &mut second);
    assert_eq!(second["data"]["repeated_unchanged"]["count"], 2, "{second}");
}

#[test]
fn a_missing_section_lists_the_closest_headings_and_says_the_list_is_partial() {
    let dir = tempfile::tempdir().unwrap();
    let mut doc = String::from("# 개발자 매뉴얼\n\n");
    for title in [
        "설치",
        "실행 흐름",
        "설정 파일",
        "기억 저장소",
        "도구 목록",
        "웹 API",
        "검토 단계",
        "완료 조건",
        "오류 처리와 복구",
        "테스트",
        "배포",
        "부록",
    ] {
        doc.push_str(&format!("## {title}\n\n내용.\n\n"));
    }
    std::fs::write(dir.path().join("summary.md"), &doc).unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    let result = tools::run_call(
        &mut s,
        &call_id(
            "document_inspect",
            json!({"section":"## 오류 처리 및 복구"}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.starts_with(r###"section_not_found: "## 오류 처리 및 복구""###),
        "{error}"
    );
    assert!(
        error.contains("The 8 closest of 13 headings (partial list; document_inspect without section shows all)"),
        "{error}"
    );
    let listed = error.split_once("shows all): ").unwrap().1;
    let candidates: Value = serde_json::from_str(listed).unwrap();
    assert_eq!(candidates.as_array().unwrap().len(), 8);
    assert_eq!(
        candidates[0]["heading"], "## 오류 처리와 복구",
        "{candidates}"
    );
}

#[test]
fn a_refusal_repeated_while_its_condition_holds_is_marked() {
    let mut tracker = tools::recovery::FailureTracker::default();
    let withheld = || {
        tools::envelope(Err(anyhow::anyhow!(
            "closing_mode: memory_read is withheld while the document is finalized"
        )))
    };
    let mut first = withheld();
    tracker.mark_repeated_failure("memory_read", r#"{"id":"m1"}"#, &mut first);
    let mut second = withheld();
    tracker.mark_repeated_failure("memory_read", r#"{"id":"m1"}"#, &mut second);
    assert_eq!(second["recovery"]["class"], "unavailable");
    assert_eq!(
        second["recovery"]["repeated_unchanged"]["count"], 2,
        "{second}"
    );
    // Waiting for tool workers can succeed on a later retry.
    for _ in 0..2 {
        let mut busy = tools::envelope(Err(anyhow::anyhow!(
            "tool_worker_capacity: previous tool threads are still running"
        )));
        tracker.mark_repeated_failure("file_read", "{}", &mut busy);
        assert!(busy["recovery"]["repeated_unchanged"].is_null(), "{busy}");
    }
}

/// A project with one source file and an existing output document.
fn project_session() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/backend")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn run() {\n    helper();\n}\n\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/backend/api.rs"), "fn api() {}\n").unwrap();
    std::fs::write(
        dir.path().join("summary.md"),
        "# Title\n\nBody.\n\n## Part\n\nMore.\n",
    )
    .unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    s.project.output = dir.path().join("summary.md");
    (dir, s)
}

#[test]
fn unaccepted_fields_name_the_field_the_call_most_likely_meant() {
    let (_dir, mut s) = project_session();
    let mut check = |name: &str, args: Value, field: &str, meant: Option<&str>, says: &str| {
        let result = tools::run_call(&mut s, &call_id(name, args));
        let error = result["error"].as_str().unwrap();
        assert!(error.starts_with("unknown_argument: "), "{error}");
        assert!(error.contains(says), "{name}: {error}");
        assert_eq!(result["data"]["input_error"]["field"], field, "{result}");
        assert_eq!(
            result["data"]["input_error"]["did_you_mean"].as_str(),
            meant,
            "{result}"
        );
        assert_eq!(result["data"]["execution"], "not_started");
    };
    check(
        "file_read",
        json!({"file_path":"src/main.rs"}),
        "file_path",
        Some("path"),
        "did you mean path? send this value as path",
    );
    // The line count is computed from the sibling start_line.
    check(
        "file_read",
        json!({"path":"src/main.rs","start_line":2,"end_line":4}),
        "end_line",
        Some("max_lines"),
        "(here max_lines: 3)",
    );
    // An array value means the array-typed queries, not query.
    check(
        "source_search",
        json!({"querys":["helper"]}),
        "querys",
        Some("queries"),
        "did you mean queries?",
    );
    check(
        "source_search",
        json!({"query":"helper","context":3}),
        "context",
        None,
        "send before and after",
    );
    check(
        "source_search",
        json!({"query":"helper","ignore_case":true}),
        "ignore_case",
        Some("case_sensitive"),
        "case_sensitive:false",
    );
    check(
        "file_edit",
        json!({"path":"src/main.rs","old_string":"a","new_text":"b","expected_hash":"x"}),
        "old_string",
        Some("old_text"),
        "did you mean old_text?",
    );
    check(
        "symbol_read",
        json!({"path":"src/main.rs","name":"helper"}),
        "name",
        None,
        r#"symbol_search {"query":"helper","match":"exact"}"#,
    );
    check(
        "document_edit_batch",
        json!({"expected_hash":"x","edits":[{"action":"replace_text","old_text":"Body.","new":"X"}]}),
        "edits[0].new",
        Some("text"),
        "did you mean text?",
    );
    // A name already supplied is not suggested twice.
    check(
        "source_search",
        json!({"query":"helper","q":"helper"}),
        "q",
        Some("query"),
        "query is already supplied, so drop q",
    );
    // Nothing similar: the allowed list stands alone.
    check(
        "file_read",
        json!({"path":"src/main.rs","zebra":1}),
        "zebra",
        None,
        "zebra is not accepted; allowed arguments:",
    );

    // A misnamed field beside a missing required one is named as its carrier.
    let result = tools::run_call(
        &mut s,
        &call_id(
            "file_patch",
            json!({"operations":[{"action":"update","path":"src/main.rs","old_string":"a","new_text":"b","to":"x"}]}),
        ),
    );
    assert!(result["error"].is_string(), "{result}");

    let result = tools::run_call(
        &mut s,
        &call_id(
            "task_state",
            json!({"action":"update","patch":{"todos":["x"]}}),
        ),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("managed by task_plan"),
        "{result}"
    );
    let result = tools::run_call(
        &mut s,
        &call_id(
            "task_plan",
            json!({"action":"apply","expected_revision":0,"texts":["x"]}),
        ),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("texts belongs inside one plan operation"),
        "{result}"
    );
}

#[test]
fn rejected_values_name_the_value_the_call_most_likely_meant() {
    let (_dir, mut s) = project_session();
    let mut check = |name: &str, args: Value, meant: Option<&str>, says: &str| {
        let result = tools::run_call(&mut s, &call_id(name, args));
        let error = result["error"].as_str().unwrap();
        assert!(error.starts_with("invalid_argument_value: "), "{error}");
        assert!(error.contains(says), "{name}: {error}");
        assert_eq!(
            result["data"]["input_error"]["did_you_mean"].as_str(),
            meant,
            "{result}"
        );
    };
    check(
        "code_outline",
        json!({"path":"src/main.rs","kind":"fn"}),
        Some("function"),
        r#"did you mean "function"?"#,
    );
    check(
        "symbol_relations",
        json!({"path":"src/main.rs","symbol_id":"x","relation":"callees"}),
        Some("calls"),
        r#"did you mean "calls"?"#,
    );
    check(
        "task_state",
        json!({"action":"update","patch":{"phase":"verification"}}),
        Some("verify"),
        r#"did you mean "verify"?"#,
    );
    check(
        "memory_manage",
        json!({"action":"list"}),
        Some("candidates"),
        r#"did you mean "candidates"?"#,
    );
    check(
        "file_patch",
        json!({"operations":[{"action":"create","path":"new.txt","content":"x"}]}),
        Some("add"),
        r#"did you mean "add"?"#,
    );
    // Document actions follow the anchor arguments sent with them.
    check(
        "document_edit",
        json!({"action":"replace","old_text":"Body.","text":"X"}),
        Some("replace_text"),
        r#"did you mean "replace_text"?"#,
    );
    check(
        "document_edit",
        json!({"action":"replace","section":"## Part","text":"## Part\n\nX\n"}),
        Some("section"),
        "needs expected_section_hash",
    );
    check(
        "document_edit_batch",
        json!({"expected_hash":"x","edits":[{"action":"create","text":"X"}]}),
        None,
        "create it first with document_edit action=create",
    );
    check(
        "code_outline",
        json!({"path":"src/main.rs","match":"regex","query":"h.*"}),
        None,
        "source_search with regex:true",
    );
    check(
        "history",
        json!({"action":"zebra"}),
        None,
        "one of: search, read",
    );
}

#[test]
fn a_field_of_another_action_names_that_action() {
    let (_dir, mut s) = project_session();
    for (name, args, says) in [
        (
            "history",
            json!({"action":"search","query":"x","id":3}),
            "id is used by action=read",
        ),
        (
            "task_state",
            json!({"action":"read","patch":{"phase":"verify"}}),
            "patch is used by action=update",
        ),
        (
            "memory_manage",
            json!({"action":"candidates","ids":["m1"]}),
            "ids is used by action=delete, action=replace",
        ),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, args));
        let error = result["error"].as_str().unwrap();
        assert!(error.starts_with("invalid_action_arguments: "), "{error}");
        assert!(error.contains(says), "{error}");
    }
}

#[test]
fn argument_errors_list_the_failing_tool_first_among_recovery_tools() {
    let (_dir, mut s) = project_session();
    s.config.memory_reuse = true;
    // memory_read and memory_find once received only "history".
    for (name, args) in [
        ("memory_read", json!({"key":"m1"})),
        ("memory_find", json!({"q":"x"})),
        ("tool_catalog", json!({"name":"file"})),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, args));
        assert_eq!(result["recovery"]["action"], "correct_arguments");
        assert_eq!(result["recovery"]["tools"][0], name, "{result}");
    }
    // A corrected path is resent to the same tool.
    let result = tools::run_call(&mut s, &call_id("file_read", json!({"path":"src/mian.rs"})));
    assert!(
        result["recovery"]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("file_read")),
        "{result}"
    );
}

#[test]
fn a_path_holding_a_citation_glob_or_misplaced_directory_says_so() {
    let (_dir, mut s) = project_session();
    let error = |s: &mut Session, name: &str, args: Value| {
        tools::run_call(s, &call_id(name, args))["error"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for path in ["src/main.rs:2-4", "src/main.rs#L2-L4", "src/main.rs:2-4:7"] {
        let message = error(&mut s, "file_read", json!({"path":path}));
        assert!(
            message.contains(
                r#"send only the file path "src/main.rs" in path and the lines separately (file_read: start_line=2, max_lines=3). "src/main.rs" exists."#
            ),
            "{path}: {message}"
        );
    }
    let message = error(
        &mut s,
        "source_search",
        json!({"query":"helper","path":"src/*.rs"}),
    );
    assert!(
        message.contains(r#"send path_glob:"src/*.rs" instead"#),
        "{message}"
    );
    // A directory named at the wrong level is found by its name.
    let message = error(&mut s, "file_list", json!({"path":"backend"}));
    assert!(
        message.contains("similar name: src/backend/. Copy one exactly"),
        "{message}"
    );
}

#[test]
fn every_missing_field_of_an_edit_action_is_named_together() {
    let (_dir, mut s) = project_session();
    let result = tools::run_call(
        &mut s,
        &call_id("document_edit", json!({"action":"replace_text"})),
    );
    assert_eq!(
        result["error"],
        "missing_argument: text for document_edit action=replace_text; old_text is also missing; action=replace_text needs text, old_text"
    );
    assert_eq!(result["data"]["input_error"]["field"], "text");
    let result = tools::run_call(
        &mut s,
        &call_id(
            "document_edit_batch",
            json!({"expected_hash":"x","edits":[{"action":"section","text":"## Part\n"}]}),
        ),
    );
    assert_eq!(
        result["error"],
        "missing_argument: edits[0].section is required for action=section; expected_section_hash is also missing; action=section needs text, section, expected_section_hash"
    );

    // file_patch operations name the action and where a field belongs.
    for (operation, says) in [
        (
            json!({"action":"add","path":"new.txt","content":"x","old_text":"y"}),
            "old_text is not accepted by action=add, which takes path, content; old_text/new_text belong to action=update",
        ),
        (
            json!({"action":"add","path":"new.txt"}),
            "missing_argument: content is required for action=add",
        ),
        (
            json!({"action":"add","path":"src/main.rs","content":"x"}),
            "already exists and action=add only creates new files; to change it use update",
        ),
    ] {
        let result = tools::run_call(
            &mut s,
            &call_id("file_patch", json!({"operations":[operation]})),
        );
        let error = result["error"].as_str().unwrap();
        assert!(error.contains(says), "{error}");
        assert!(error.ends_with("no changes persisted"), "{error}");
    }
}

#[test]
fn a_malformed_hash_is_not_reported_as_a_changed_document() {
    let (dir, mut s) = project_session();
    let before = std::fs::read(dir.path().join("summary.md")).unwrap();
    // The edit's own problem comes with the hash error, as for a missing hash.
    let result = tools::run_call(
        &mut s,
        &call_id(
            "document_edit",
            json!({"action":"insert_after","section":"## Nope","text":"## New\n\nx\n","expected_hash":"abc"}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("expected_hash is not a document hash"),
        "{error}"
    );
    assert!(
        error.contains(
            "the edit was also checked against the current document and failed: section_not_found"
        ),
        "{error}"
    );
    assert!(error.ends_with("nothing was written"), "{error}");
    let result = tools::run_call(
        &mut s,
        &call_id(
            "document_edit_batch",
            json!({"expected_hash":"abc","edits":[{"action":"replace_text","old_text":"Missing","text":"x"}]}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("its edits were also checked against the current document and 1 failed: [index=0; action=replace_text; cause=patch_target_must_match_once"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("summary.md")).unwrap(),
        before
    );
    // Paging with a value no tool issued does not claim the document changed.
    let result = tools::run_call(
        &mut s,
        &call_id(
            "document_inspect",
            json!({"offset":1,"expected_hash":"bad"}),
        ),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("expected_hash is not a document hash"),
        "{error}"
    );
    assert!(!error.contains("changed"), "{error}");
}

#[test]
fn compacted_input_errors_keep_the_suggested_field() {
    let mut s = session();
    let invocation = call("file_read", json!({"file_path":"src/main.rs"}));
    let mut result = tools::run_call(&mut s, &invocation);
    result["error"] = json!(format!("unknown_argument: {}", "details ".repeat(1000)));
    let limited = tools::limit_result(&mut s, &invocation, result, 200);
    assert_eq!(
        limited["data"]["input_error"]["did_you_mean"], "path",
        "{limited}"
    );
}

#[test]
fn a_directory_path_with_a_glob_names_the_one_glob_that_means_both() {
    let (dir, mut s) = project_session();
    std::fs::write(dir.path().join("src/backend/style.css"), "a {}\n").unwrap();
    let absolute = dir.path().join("src/backend").display().to_string();
    // A live run resent this file_list five times without the combined form.
    for (name, args, says) in [
        (
            "file_list",
            json!({"path":absolute,"path_glob":"*.css"}),
            r#"send only path_glob:"src/backend/**/*.css""#,
        ),
        (
            "source_search",
            json!({"query":"a","path":"src","pattern":"backend/*.rs"}),
            r#"send only path_glob:"src/backend/*.rs""#,
        ),
        (
            "symbol_search",
            json!({"query":"api","path":"src","path_glob":"*.rs"}),
            r#"send only path_glob:"src/**/*.rs""#,
        ),
        (
            "source_search",
            json!({"query":"a","path":"src/main.rs","path_glob":"*.rs"}),
            "path already names one file, so drop path_glob",
        ),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, args));
        let error = result["error"].as_str().unwrap();
        assert!(error.contains(says), "{name}: {error}");
    }
    // The suggested glob works.
    let result = tools::run_call(
        &mut s,
        &call_id("file_list", json!({"path_glob":"src/backend/**/*.css"})),
    );
    assert_eq!(
        result["data"]["paths"],
        json!(["src/backend/style.css"]),
        "{result}"
    );
}

#[test]
fn a_path_naming_the_project_root_beside_a_glob_is_no_conflict() {
    // A live model sent the root's absolute path with path_glob and was
    // refused for conflicting filters; a glob is relative to the root.
    let (dir, mut s) = project_session();
    std::fs::write(dir.path().join("src/backend/style.css"), "a {}\n").unwrap();
    let root = dir.path().display().to_string();
    for path in [root.as_str(), ".", "./"] {
        let result = tools::run_call(
            &mut s,
            &call_id(
                "file_list",
                json!({"path":path,"path_glob":"src/backend/*.css","mode":"paths"}),
            ),
        );
        assert_eq!(
            result["data"]["paths"],
            json!(["src/backend/style.css"]),
            "{path}: {result}"
        );
    }
    for (name, args) in [
        (
            "source_search",
            json!({"query":"fn api","path":root,"path_glob":"src/**/*.rs"}),
        ),
        (
            "symbol_search",
            json!({"query":"api","path":root,"path_glob":"src/**/*.rs"}),
        ),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, args));
        assert_eq!(result["status"], "ok", "{name}: {result}");
        assert!(
            result.to_string().contains("src/backend/api.rs"),
            "{name}: {result}"
        );
    }
    // A subdirectory with a glob still names the one glob meaning both.
    let result = tools::run_call(
        &mut s,
        &call_id("file_list", json!({"path":"src","path_glob":"*.css"})),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("send only path_glob"),
        "{result}"
    );
}

#[test]
fn a_directory_given_to_a_file_tool_names_that_tool_for_the_next_call() {
    // A live model sent code_outline the project root twice and was told to
    // use file_read.
    let (dir, mut s) = project_session();
    s.active_tools = ToolRegistry::optional_names();
    let root = dir.path().display().to_string();
    for tool in ["code_outline", "file_read"] {
        let error = tools::execute(&mut s, tool, json!({"path":root}))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("path_is_directory"), "{tool}: {error}");
        assert!(
            error.contains(&format!("then call {tool} with that file path")),
            "{tool}: {error}"
        );
    }
}

#[test]
fn an_unknown_tool_name_names_the_offered_tool_it_most_likely_meant() {
    let (_dir, mut s) = project_session();
    for (name, meant, says) in [
        ("read", Some("file_read"), "did you mean file_read?"),
        ("tool_plan", Some("task_plan"), "did you mean task_plan?"),
        (
            "read_result_key>main.jsx</arg_value>",
            Some("file_read"),
            "carries extra text or call markup",
        ),
        ("run_guidance", None, "run_guidance is program state"),
        (
            "zebra",
            None,
            "zebra is not a tool name; nothing was executed. Use",
        ),
    ] {
        let result = tools::run_call(&mut s, &call_id(name, json!({"path":"src/main.rs"})));
        assert_eq!(result["recovery"]["code"], "unsupported_tool", "{result}");
        let error = result["error"].as_str().unwrap();
        assert!(error.contains(says), "{name}: {error}");
        assert_eq!(result["data"]["did_you_mean"].as_str(), meant, "{result}");
        if let Some(meant) = meant {
            assert_eq!(result["recovery"]["tools"][0], meant, "{result}");
        }
    }
}

#[test]
fn blank_symbol_cursors_and_paths_start_from_the_beginning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
    let mut s = session();
    s.project.root = dir.path().into();
    // The live shape: every optional string filled with "". The empty query
    // stays an exact empty-name filter; the blank cursor is dropped.
    let outline = tools::execute(
        &mut s,
        "code_outline",
        json!({"path":"a.rs","query":"","match":"contains","case_sensitive":true,"kind":"function","container":"","max_depth":1,"view":"compact","cursor":"","limit":100}),
    )
    .unwrap();
    assert!(outline.to_string().contains("helper"), "{outline}");
    let found = tools::execute(
        &mut s,
        "symbol_search",
        json!({"query":"helper","path":"","path_glob":"","cursor":""}),
    )
    .unwrap();
    assert!(found.to_string().contains("helper"), "{found}");
    // A live run scoped the search with path_glob and filled the legacy
    // pattern alias with "": that is no second filter.
    let scoped = tools::execute(
        &mut s,
        "symbol_search",
        json!({"query":"helper","path_glob":"a.rs","pattern":"","cursor":"","limit":30,"path":"","match":"exact","case_sensitive":true,"kind":"function","container":"","max_depth":2}),
    )
    .unwrap();
    assert!(scoped.to_string().contains("helper"), "{scoped}");
}
