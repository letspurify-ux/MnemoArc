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

#[test]
fn cleanup_output_reservation_is_bounded_so_large_outputs_keep_input_room() {
    use mnemoarc::context::{CLEANUP_OUTPUT_CAP, ContextManager};
    let large = Config {
        context_tokens: 230_000,
        output_tokens: 32_000,
        ..Default::default()
    };
    assert_eq!(ContextManager::cleanup_output_tokens(&large), CLEANUP_OUTPUT_CAP);
    // One full response plus three bounded cleanup rounds, not four outputs.
    let budget = ContextManager::input_budget(&large);
    assert!(budget > 130_000, "{budget}");
    let small = Config {
        context_tokens: 64_000,
        output_tokens: 8_000,
        ..Default::default()
    };
    assert_eq!(ContextManager::cleanup_output_tokens(&small), 8_000);
}

/// Scripted document run: two identical outline requests, then a final.
/// Records each request's run_guidance for assertions.
struct Scripted {
    steps: Mutex<Vec<Completion>>,
    guidance: Mutex<Vec<Value>>,
}

#[async_trait]
impl LlmClient for Scripted {
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
        self.guidance
            .lock()
            .unwrap()
            .push(state["run_guidance"].clone());
        let mut steps = self.steps.lock().unwrap();
        if steps.is_empty() {
            anyhow::bail!("script_exhausted");
        }
        Ok(steps.remove(0))
    }
}

fn call(id: &str, name: &str, args: Value) -> Completion {
    Completion {
        calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }],
        ..Default::default()
    }
}

fn verified_fixture() -> (tempfile::TempDir, Session) {
    let (dir, mut s, source) = fixture();
    s.config.source_document_review = false;
    s.config.completion_review_enabled = false;
    for id in ["flow", "history"] {
        tools::execute(&mut s, "investigation", json!({"action":"verify","id":id,"source_ids":[source],"verification_note":"Compared the cited lines with the section"})).unwrap();
    }
    (dir, s)
}

async fn run_scripted(s: Session, steps: Vec<Completion>) -> (Session, Vec<Value>) {
    let client = Arc::new(Scripted {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let guidance = client.guidance.lock().unwrap().clone();
    (result, guidance)
}

#[tokio::test]
async fn unchanged_outline_repeat_returns_a_short_marker() {
    let (_dir, s) = verified_fixture();
    let (result, _) = run_scripted(
        s,
        vec![
            call("inspect-1", "document_inspect", json!({})),
            call("inspect-2", "document_inspect", json!({})),
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    let outputs: Vec<Value> = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(outputs.len(), 2);
    assert!(outputs[0]["data"]["outline"].is_array());
    assert_eq!(outputs[1]["data"]["unchanged"], true);
    assert!(outputs[1]["data"]["outline"].is_null());
}

#[tokio::test]
async fn finished_bookkeeping_asks_for_the_final_answer() {
    let (_dir, s) = verified_fixture();
    let (result, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(guidance[0]["ready_for_final"], true);
    assert!(
        guidance[0]["instruction"]
            .as_str()
            .unwrap()
            .starts_with("Ready to finish")
    );

    // An unverified item keeps the ordinary guidance.
    let (_dir, s, _) = fixture();
    let (_, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert!(guidance[0]["ready_for_final"].is_null());
}

#[test]
fn provider_usage_calibrates_estimated_token_counts() {
    use mnemoarc::context::ContextManager;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "unknown-provider-model".into(),
            model_context: Some(140_000),
            context_tokens: 140_000,
            output_tokens: 12_000,
            ..Default::default()
        },
    );
    assert_eq!(ContextManager::token_ratio(&s), 1.0);
    // Enough complete history groups to evict.
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2000))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    let request = budget * 85 / 100;
    // Above the 80% high-water mark by the uncalibrated estimate.
    let mut uncalibrated = s.clone();
    assert!(ContextManager::prepare(&mut uncalibrated, request).unwrap());
    // The provider measured 20% fewer tokens: the same request fits.
    for _ in 0..3 {
        ContextManager::record_usage(&mut s, 10_000, 8_000);
    }
    assert!((ContextManager::token_ratio(&s) - 0.8).abs() < 1e-9);
    assert_eq!(ContextManager::calibrated(&s, 10_000), 8_000);
    assert!(!ContextManager::prepare(&mut s, request).unwrap());
    // The highest recent ratio wins, so a larger measurement is never hidden.
    ContextManager::record_usage(&mut s, 10_000, 11_000);
    assert!((ContextManager::token_ratio(&s) - 1.1).abs() < 1e-9);
    // Known tokenizers are exact and never calibrated.
    s.config.model = "gpt-4o".into();
    ContextManager::record_usage(&mut s, 10_000, 5_000);
    assert_eq!(ContextManager::token_ratio(&s), 1.0);
}

#[tokio::test]
async fn rejected_review_asks_for_one_batched_repair() {
    let (_dir, mut s, _) = fixture();
    s.config.completion_review_enabled = false;
    s.document_review.issues = vec![
        "Flow: state the loop bound".into(),
        "History: name the helper".into(),
    ];
    let (_, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert_eq!(guidance[0]["review_repair"]["findings"], 2);
    assert!(
        guidance[0]["instruction"]
            .as_str()
            .unwrap()
            .starts_with("Review repair: fix ALL findings")
    );

    // Once repairs are verified, the final-answer guidance names the open
    // findings so each one is confirmed before a costly re-review.
    let (_dir, mut s) = verified_fixture();
    s.document_review.issues = vec!["Flow: state the loop bound".into()];
    let (_, guidance) = run_scripted(
        s,
        vec![Completion {
            text: "Saved out.md".into(),
            ..Default::default()
        }],
    )
    .await;
    assert_eq!(guidance[0]["ready_for_final"], true);
    assert!(
        guidance[0]["instruction"]
            .as_str()
            .unwrap()
            .contains("all 1 findings in document_review.issues")
    );
}

#[tokio::test]
async fn audits_during_review_repair_return_a_short_page() {
    let (dir, mut s, _) = fixture();
    s.config.completion_review_enabled = false;
    s.config.source_document_review = false;
    s.document_review.issues = vec!["Flow: fix citations".into()];
    // Seven citations to a file that does not exist: seven audit issues.
    let hash = tools::hash(&std::fs::read(dir.path().join("out.md")).unwrap());
    let broken: String = (1..=7)
        .map(|i| format!("Claim {i}. missing.js:{i}-{i}\n"))
        .collect();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":format!("\n# Extra\n{broken}")}),
    )
    .unwrap();
    let (result, _) = run_scripted(
        s,
        vec![call("audit", "document_audit", json!({}))],
    )
    .await;
    let output: Value = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
        .next()
        .unwrap();
    assert_eq!(output["data"]["compacted_for_review_repair"], true, "{output}");
    assert_eq!(output["data"]["issues"].as_array().unwrap().len(), 5);
    assert_eq!(output["data"]["next_offset"], 5);
    assert!(output["data"]["issue_count"].as_u64().unwrap() > 5);
}

#[test]
fn closing_without_a_document_withholds_reading_until_it_is_written() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), "run();\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..Default::default()
        },
    );
    s.add_user("Write out.md about main.js".into());
    s.active_tools = ToolRegistry::optional_names();
    s.progress_recovery.closing = Some(Closing::default());
    let names = tool_names(&s);
    for withheld in ["file_read", "symbol_read", "source_search"] {
        assert!(!names.iter().any(|name| name == withheld), "{withheld}");
    }
    assert!(names.iter().any(|name| name == "document_edit"));
    let error = tools::execute(&mut s, "file_read", json!({"path":"main.js"}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("withheld until the document is saved"), "{error}");
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Main\nIt runs. main.js:1-1\n"}),
    )
    .unwrap();
    // With a saved document, a targeted read of a cited range is allowed again.
    assert!(tool_names(&s).iter().any(|name| name == "file_read"));
    tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
}

#[tokio::test]
async fn reading_new_sources_before_the_first_write_is_progress() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), numbered(40)).unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            output_tokens: 1_024,
            stall_round_limit: 2,
            source_answer_review: false,
            ..Default::default()
        },
    );
    s.add_user("Document main.js with source evidence".into());
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":"source_document"}}),
    )
    .unwrap();
    // Twelve distinct ranges: four times the closing threshold (3 x 2).
    let mut steps: Vec<Completion> = (0..12)
        .map(|i| {
            call(
                &format!("read-{i}"),
                "file_read",
                json!({"path":"main.js","start_line":i * 3 + 1,"max_lines":3}),
            )
        })
        .collect();
    steps.push(call(
        "write",
        "document_edit",
        json!({"action":"create","text":"# Main\nForty values. main.js:1-40\n"}),
    ));
    let (result, guidance) = run_scripted(s, steps).await;
    assert!(
        result.document_written,
        "{:?} {:?}",
        result.status,
        result.last_error
    );
    assert!(
        guidance.iter().take(13).all(|g| g["closing"].is_null()),
        "{:?}",
        guidance.iter().map(|g| &g["closing"]).collect::<Vec<_>>()
    );
    // The guidance still moved to drafting; only closing was not triggered.
    assert!(guidance.iter().any(|g| g["phase"] == "draft"));
}

fn numbered(lines: usize) -> String {
    (1..=lines).map(|i| format!("let v{i} = {i};\n")).collect()
}
