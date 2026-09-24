//! Closing mode: document work converges on a finished result, reporting
//! unresolved items instead of repeating until the run budget is spent.
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall, Usage},
    session::{Closing, Session},
    tools::{self, ToolRegistry, document_review},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, Session, Value) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.js"),
        "function run(history) {\n  const turns = normalize(history);\n  for (let i = 0; i < 5; i++) {\n    work(turns);\n  }\n}\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            source_answer_review: false,
            ..Default::default()
        },
    );
    s.add_user("Write a source document covering the loop and history handling.".into());
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":"source_document"}}),
    )
    .unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Flow\nA for loop runs work five times. main.js:3-5\n\n# History\nHistory is normalized first. main.js:2-2\n"}),
    )
    .unwrap();
    for (id, section) in [("flow", "# Flow"), ("history", "# History")] {
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":id,"section":section,"status":"written"}),
        )
        .unwrap();
    }
    (dir, s, read["source"]["id"].clone())
}

fn tool_names(s: &Session) -> Vec<String> {
    ToolRegistry::definitions(s)
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn mark_gap_is_offered_and_accepted_only_in_closing_mode() {
    let (_dir, mut s, source) = fixture();
    let investigation = ToolRegistry::definitions(&s)
        .into_iter()
        .find(|tool| tool["function"]["name"] == "investigation")
        .unwrap();
    assert!(!investigation.to_string().contains("mark_gap"));
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"mark_gap","id":"history","reason":"The normalize helper body is outside the delivered sources"}),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("gap_requires_closing"));

    s.progress_recovery.closing = Some(Closing::default());
    let investigation = ToolRegistry::definitions(&s)
        .into_iter()
        .find(|tool| tool["function"]["name"] == "investigation")
        .unwrap();
    assert!(investigation.to_string().contains("mark_gap"));
    let short = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"mark_gap","id":"history","reason":"unknown"}),
    );
    assert!(short.is_err());
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"mark_gap","id":"history","reason":"The normalize helper body is outside the delivered sources"}),
    )
    .unwrap();
    let item = s.investigations.iter().find(|i| i.id == "history").unwrap();
    assert_eq!(item.status, "gap");
    assert!(item.is_settled());

    // A verified item needs no gap; the remaining item still blocks.
    tools::execute(&mut s, "investigation", json!({"action":"verify","id":"flow","source_ids":[source],"verification_note":"Compared the for loop and its bound with the document"})).unwrap();
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"mark_gap","id":"flow","reason":"Attempting to hide a verified item"}),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("item_already_verified"));
    let check = tools::execute(&mut s, "investigation", json!({"action":"final_check"})).unwrap();
    assert_eq!(check["complete"], true, "{check}");

    // Editing the gap's section does not silently turn it back into work,
    // but a later verification replaces the gap with real evidence.
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let edit = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"History is normalized first.","text":"History is normalized first (unconfirmed helper)."}),
    )
    .unwrap();
    assert_eq!(edit["verification_required_ids"], json!([]));
    let item = s.investigations.iter().find(|i| i.id == "history").unwrap();
    assert_eq!(item.status, "gap");
    tools::execute(&mut s, "investigation", json!({"action":"verify","id":"history","source_ids":[source],"verification_note":"Compared the normalize call on line 2 with the history section"})).unwrap();
    let item = s.investigations.iter().find(|i| i.id == "history").unwrap();
    assert_eq!(item.status, "verified");
}

#[test]
fn closing_mode_and_second_stall_stage_withhold_discovery_tools() {
    let (_dir, mut s, _) = fixture();
    s.active_tools = ToolRegistry::optional_names();
    let names = tool_names(&s);
    assert!(names.iter().any(|name| name == "source_search"));

    // Twice the stall limit without a better result narrows discovery.
    s.progress_recovery.rounds_since_best = s.config.stall_round_limit * 2;
    let names = tool_names(&s);
    assert!(!names.iter().any(|name| name == "source_search"));
    assert!(names.iter().any(|name| name == "file_read"));
    s.progress_recovery.rounds_since_best = 0;

    s.progress_recovery.closing = Some(Closing::default());
    let names = tool_names(&s);
    for blocked in ["file_list", "source_search", "symbol_search", "code_outline", "memory_write"] {
        assert!(!names.iter().any(|name| name == blocked), "{blocked}");
    }
    for kept in ["file_read", "document_edit", "investigation", "task_plan"] {
        assert!(names.iter().any(|name| name == kept), "{kept}");
    }
    let error = tools::execute(&mut s, "source_search", json!({"query":"normalize"})).unwrap_err();
    assert!(error.to_string().starts_with("closing_mode"));
    let result = tools::run_call(
        &mut s,
        &ToolCall {
            id: "closing-search".into(),
            name: "source_search".into(),
            arguments: json!({"query":"normalize"}).to_string(),
        },
    );
    assert_eq!(result["recovery"]["class"], "unavailable");
    assert!(tools::recovery::correctable_document_error(&result));
}

#[test]
fn rereview_lists_previous_findings_and_changed_sections() {
    let (_dir, mut s, _) = fixture();
    let first = document_review::request(&mut s).unwrap();
    assert_eq!(first["response_format"]["type"], "json_schema");
    let payload: Value =
        serde_json::from_str(first["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["previous_findings"], json!([]));
    assert!(payload["changed_sections"].is_null());
    document_review::finish(&mut s, r#"{"issues":["History: name the helper"]}"#).unwrap();
    assert!(!s.document_review.pending);

    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"History is normalized first.","text":"History is normalized by normalize() first."}),
    )
    .unwrap();
    let second = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(second["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["previous_findings"],
        json!([{"id":"F1","text":"History: name the helper"}])
    );
    assert_eq!(payload["changed_sections"], json!(["# History"]));
    assert!(
        second["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("RE-REVIEW")
    );
}

#[test]
fn unavailable_review_applies_only_to_the_unchanged_document() {
    let (_dir, mut s, _) = fixture();
    document_review::request(&mut s).unwrap();
    s.document_review.pending = true;
    document_review::mark_unavailable(&mut s);
    assert!(!s.document_review.pending);
    assert!(document_review::unavailable_on_current(&s));
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"\nMore detail.\n"}),
    )
    .unwrap();
    assert!(!document_review::unavailable_on_current(&s));
}

/// Appends a new paragraph each request (real progress) until closing mode is
/// announced, then answers. Records the tools offered during closing.
struct BudgetWriter {
    calls: Mutex<usize>,
    closing_tools: Mutex<Option<Vec<String>>>,
    path: std::path::PathBuf,
}

#[async_trait]
impl LlmClient for BudgetWriter {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        assert!(*calls < 80, "closing did not finish the run");
        let usage = Some(Usage {
            input: 20_000,
            output: 10,
            cached: None,
        });
        if state["run_guidance"]["closing"]["active"] == true {
            assert_eq!(state["run_guidance"]["closing"]["reason"], "budget");
            assert!(
                state["run_guidance"]["instruction"]
                    .as_str()
                    .unwrap()
                    .starts_with("Closing mode")
            );
            *self.closing_tools.lock().unwrap() = Some(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                    .collect(),
            );
            return Ok(Completion {
                text: "Saved the document.".into(),
                usage,
                ..Default::default()
            });
        }
        let previous = std::fs::read(&self.path)?;
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("append-{calls}"),
                name: "document_edit".into(),
                arguments: json!({"action":"append","expected_hash":tools::hash(&previous),"text":format!("\nParagraph {calls}.\n")}).to_string(),
            }],
            usage,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn closing_reserve_finishes_steady_work_before_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.md");
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: output.clone(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            output_tokens: 1024,
            run_tokens: 1_000_000,
            source_answer_review: false,
            source_document_review: false,
            completion_review_enabled: false,
            ..Default::default()
        },
    );
    s.add_user("Write out.md".into());
    s.active_tools = ToolRegistry::optional_names();
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":"document_edit"}}),
    )
    .unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Result\nIntro.\n"}),
    )
    .unwrap();
    let client = Arc::new(BudgetWriter {
        calls: Mutex::new(0),
        closing_tools: Mutex::new(None),
        path: output,
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut deltas = vec![];
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Delta { text, .. } = event {
                deltas.push(text);
            }
        }
        deltas
    });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    let deltas = drain.await.unwrap();
    // Steady progress never trips the stall ladder; the 10% reserve starts
    // closing, and a clean final there completes normally.
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.completion_gaps.is_empty());
    assert_eq!(deltas, ["Saved the document."]);
    let closing_tools = client.closing_tools.lock().unwrap().clone().unwrap();
    assert!(!closing_tools.iter().any(|name| name == "source_search"));
    assert!(closing_tools.iter().any(|name| name == "document_edit"));
    let spent = result.input_tokens + result.output_tokens;
    assert!((900_000..1_000_000).contains(&spent), "{spent}");
}
