use crate::support;
use mnemoarc::{
    config::Project,
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
        support::compact_config(),
    );
    session.active_tools = ToolRegistry::optional_names();
    session
}

#[test]
fn a_blank_file_read_path_says_how_to_read_the_output() {
    // A provider that fills every field sent path "" to read the output and
    // got only "missing_argument: path".
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    current.project.output = outside.path().join("generated.md");
    let error = tools::execute(
        &mut current,
        "file_read",
        json!({"path":"","cursor":"","start_line":1,"max_lines":5,"limit":5,"force_read":false}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("missing_argument: path"), "{error}");
    assert!(
        error.contains("document_inspect") && error.contains("generated.md"),
        "{error}"
    );
}

#[test]
fn a_file_read_without_a_path_after_an_edit_is_told_the_edit_already_confirmed_it() {
    // GLM sent {max_lines, force_read:true} with no path five times, four of
    // them right after a document edit, apparently to look at the output
    // again. The description had said to read the output "without supplying
    // a path" next to the file_read rules.
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    current.project.output = dir.path().join("generated.md");
    let error = tools::execute(
        &mut current,
        "file_read",
        json!({"max_lines":200,"force_read":true}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("missing_argument: path"), "{error}");
    assert!(error.contains("no need to re-read it"), "{error}");
    assert!(error.contains("document_inspect"), "{error}");
    let description = ToolRegistry::specs()
        .into_iter()
        .find(|spec| spec.name == "file_read")
        .unwrap()
        .description;
    assert!(
        description.contains("file_read always needs path, or cursor alone"),
        "{description}"
    );
    assert!(
        !description.contains("without supplying a path"),
        "{description}"
    );
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
fn empty_fields_of_another_action_are_omitted() {
    // A provider that fills every field sent task_state action=read with
    // patch:{} and was refused; an empty value gives the action nothing.
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    tools::execute(
        &mut current,
        "task_state",
        json!({"action":"read","patch":{},"offset":0,"limit":20}),
    )
    .unwrap();
    tools::execute(
        &mut current,
        "memory_manage",
        json!({"action":"candidates","ids":[],"replacement":{}}),
    )
    .unwrap();
    // A filled field of another action is still refused.
    let error = tools::execute(
        &mut current,
        "task_state",
        json!({"action":"read","patch":{"phase":"verify"}}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("invalid_action_arguments:"), "{error}");
}

#[test]
fn a_task_state_call_sent_whole_as_its_patch_is_that_call() {
    // A live model sent {"patch":"{\"action\": \"update\", \"patch\": {...}}"}
    // and was refused for a missing action.
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    let whole = json!({"action":"update","patch":{"purpose":"UI manual"}});
    tools::execute(
        &mut current,
        "task_state",
        json!({"patch":whole.to_string()}),
    )
    .unwrap();
    assert_eq!(current.task.purpose, "UI manual");
    tools::execute(
        &mut current,
        "task_state",
        json!({"action":"update","patch":{"action":"update","patch":{"scope":"frontend"}}}),
    )
    .unwrap();
    assert_eq!(current.task.scope, "frontend");
    // A different outer action is not overridden.
    let error = tools::execute(
        &mut current,
        "task_state",
        json!({"action":"read","patch":{"action":"update","patch":{"scope":"x"}}}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("invalid_action_arguments:"), "{error}");
    assert_eq!(current.task.scope, "frontend");
}

#[test]
fn recovery_navigation_schemas_keep_a_provider_compatible_object_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut current = session(dir.path());
    current.document_written = true;
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

// Live run 2026-10-09: a model sent case_sensitive:"false" and
// whole_word:"true" and resent them after the type error.
#[test]
fn quoted_booleans_are_read_as_booleans() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn Execute_one() {}\nfn execute_once() {}\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    let result = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"a.rs","query":"execute_one","case_sensitive":"false","whole_word":"true"}),
    )
    .unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(result["matches"][0]["line"], 1, "{result}");
}
