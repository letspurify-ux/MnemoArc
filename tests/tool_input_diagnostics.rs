mod support;
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
                "active":s.active_tools,"pending":s.pending_tools,"investigations":s.investigations});
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
                "active":s.active_tools,"pending":s.pending_tools,"investigations":s.investigations})
            );
            checked += 1;
        }
    }
    assert!(checked > 140, "only {checked} parameters checked");
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
        (
            "investigation",
            json!({"action":"verify_batch","items":{}}),
            "items",
            json!({"minProperties":1}),
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
    let invocation = call("investigation", json!({"action":"verify_batch"}));
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
    let invocation = call("investigation", json!({"action":"verify_batch"}));
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
