use mnemoarc::{
    config::{Config, Project},
    llm::{MAX_TOOL_CALL_ID_BYTES, ToolCall},
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Map, Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn session(root: &std::path::Path) -> Session {
    let mut session = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config::default(),
    );
    session.active_tools = ToolRegistry::optional_names();
    session
}

#[test]
fn empty_optional_navigation_strings_are_omitted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::write(
        dir.path().join("frontend/src/App.jsx"),
        "const app = true;\n",
    )
    .unwrap();
    let mut current = session(dir.path());
    let listed = tools::execute(
        &mut current,
        "file_list",
        json!({"cursor":"","limit":100,"mode":"paths","path":"","path_glob":"frontend/src/*.jsx","pattern":""}),
    ).unwrap();
    assert!(
        listed.to_string().contains("frontend/src/App.jsx"),
        "{listed}"
    );
    let listed_dir = tools::execute(
        &mut current,
        "file_list",
        json!({"cursor":"","mode":"paths","path":"frontend/src","path_glob":"","pattern":""}),
    )
    .unwrap();
    assert!(
        listed_dir.to_string().contains("frontend/src/App.jsx"),
        "{listed_dir}"
    );
    let read = tools::execute(
        &mut current,
        "file_read",
        json!({"path":"frontend/src/App.jsx","cursor":"","start_line":1,"max_lines":1}),
    )
    .unwrap();
    assert!(read.to_string().contains("const app = true"), "{read}");
    let conflict = tools::execute(
        &mut current,
        "file_list",
        json!({"mode":"paths","path":"frontend/src","path_glob":"frontend/src/*.jsx"}),
    )
    .unwrap_err()
    .to_string();
    assert!(conflict.starts_with("conflicting_arguments:"), "{conflict}");
}

#[test]
fn recovery_navigation_schemas_keep_a_provider_compatible_object_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    current.document_written = true;
    current.task.require_investigation = true;
    current.run_guidance = json!({"phase":"verify","progress_recovery":{"active":true}});
    let definitions = ToolRegistry::definitions(&current);
    for name in ["file_list", "source_search"] {
        let spec = definitions
            .iter()
            .find(|tool| tool["function"]["name"] == name)
            .unwrap();
        let parameters = &spec["function"]["parameters"];
        assert_eq!(parameters["type"], "object", "{name}: {parameters}");
        assert!(parameters.get("anyOf").is_none(), "{name}: {parameters}");
    }
}

#[test]
fn oversized_call_id_is_rejected_before_tool_execution() {
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    let call = ToolCall {
        id: "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),
        name: "tool_select".into(),
        arguments: json!({"action":"add","names":["document_edit"]}).to_string(),
    };
    let result = tools::run_call(&mut current, &call);
    assert_eq!(result["status"], "error");
    assert_eq!(result["recovery"]["code"], "malformed_tool_call");
    assert!(current.pending_tools.is_none());
    assert!(current.ledger.is_empty());
}

fn placeholder(field: &Value) -> Value {
    if let Some(first) = field["enum"].as_array().and_then(|values| values.first()) {
        return first.clone();
    }
    match field["type"].as_str() {
        Some("string") => json!("x"),
        Some("integer") => json!(0),
        Some("boolean") => json!(false),
        Some("array") => json!([]),
        Some("object") => json!({}),
        _ => Value::Null,
    }
}

#[test]
fn malformed_fields_never_panic_in_any_registered_tool() {
    let dir = tempfile::tempdir().unwrap();
    let candidates = [
        Value::Null,
        json!(false),
        json!(0),
        json!(u64::MAX),
        json!(""),
        json!("x"),
        json!([]),
        json!(["x"]),
        json!({}),
        json!({"x":"y"}),
    ];
    for spec in ToolRegistry::specs() {
        let fields = spec.parameters["properties"].as_object().unwrap();
        let required = spec.parameters["required"].as_array().unwrap();
        for (key, _) in fields {
            for candidate in &candidates {
                let mut args = Map::new();
                for required_key in required {
                    let required_key = required_key.as_str().unwrap();
                    args.insert(required_key.into(), placeholder(&fields[required_key]));
                }
                args.insert(key.clone(), candidate.clone());
                let mut current = session(dir.path());
                let result = catch_unwind(AssertUnwindSafe(|| {
                    tools::execute(&mut current, spec.name, Value::Object(args))
                }));
                assert!(
                    result.is_ok(),
                    "{} panicked for {key}={candidate}",
                    spec.name
                );
            }
        }
    }
}
