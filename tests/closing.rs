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
    s.select_workflow("source_document").unwrap();
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
    for blocked in [
        "file_list",
        "source_search",
        "symbol_search",
        "code_outline",
        "memory_write",
    ] {
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
    s.select_workflow("document_edit").unwrap();
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
    assert_eq!(
        ContextManager::cleanup_output_tokens(&large),
        CLEANUP_OUTPUT_CAP
    );
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
    tools: Mutex<Vec<Vec<String>>>,
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
        // Review requests carry a JSON payload instead of the program state.
        let state: Value = request["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_str())
            .and_then(|content| content.split_once('\n'))
            .and_then(|(_, json)| serde_json::from_str(json).ok())
            .unwrap_or(Value::Null);
        self.guidance
            .lock()
            .unwrap()
            .push(state["run_guidance"].clone());
        self.tools.lock().unwrap().push(
            request["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        );
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
    let (session, guidance, _) = run_scripted_tools(s, steps).await;
    (session, guidance)
}

async fn run_scripted_tools(
    s: Session,
    steps: Vec<Completion>,
) -> (Session, Vec<Value>, Vec<Vec<String>>) {
    let client = Arc::new(Scripted {
        steps: Mutex::new(steps),
        guidance: Mutex::new(vec![]),
        tools: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let guidance = client.guidance.lock().unwrap().clone();
    let tools = client.tools.lock().unwrap().clone();
    (result, guidance, tools)
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

#[tokio::test]
async fn open_todos_on_a_finished_document_are_closed_in_one_batch() {
    let (_dir, mut s) = verified_fixture();
    // The live shape: the document is done but three plan items remain,
    // which previously cost a request each.
    tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Read sources","Write sections","Verify sections"]}]}),
    )
    .unwrap();
    let operations: Vec<Value> = ["T1", "T2", "T3"]
        .iter()
        .map(|id| json!({"op":"complete","id":id,"result":"Done in out.md"}))
        .collect();
    let (result, guidance) = run_scripted(
        s,
        vec![
            call(
                "closeout",
                "task_plan",
                json!({"action":"apply","expected_revision":1,"operations":operations}),
            ),
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    let closeout = &guidance[0]["plan_closeout"];
    assert_eq!(closeout["expected_revision"], 1);
    assert_eq!(closeout["pending_count"], 3);
    assert_eq!(closeout["items"][0]["id"], "T1");
    assert!(guidance[0]["ready_for_final"].is_null());
    assert!(
        guidance[0]["instruction"]
            .as_str()
            .unwrap()
            .contains("Close them in ONE task_plan apply")
    );
    // One batched update was enough: the next request is ready to finish.
    assert_eq!(guidance[1]["ready_for_final"], true);
    assert!(result.task.current_todo().is_none());
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
    let (result, _) = run_scripted(s, vec![call("audit", "document_audit", json!({}))]).await;
    let output: Value = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
        .next()
        .unwrap();
    assert_eq!(
        output["data"]["compacted_for_review_repair"], true,
        "{output}"
    );
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
    assert!(
        error.contains("withheld until the document is saved"),
        "{error}"
    );
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
    s.select_workflow("source_document").unwrap();
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
        result.status, result.last_error
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

#[tokio::test]
async fn discovery_tools_stay_available_until_the_document_exists() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(dir.path().join("backend/src/agent.js"), numbered(40)).unwrap();
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
    s.add_user("Document backend/src/agent.js with source evidence".into());
    s.active_tools = ToolRegistry::optional_names();
    s.select_workflow("source_document").unwrap();
    // Wrong guesses first (no new evidence), then real distinct reads: both
    // stretches exceed the stall limit before any document is written.
    let mut steps: Vec<Completion> = (0..5)
        .map(|i| {
            call(
                &format!("guess-{i}"),
                "file_read",
                json!({"path":format!("backend/agent{i}.py")}),
            )
        })
        .collect();
    steps.extend((0..6).map(|i| {
        call(
            &format!("read-{i}"),
            "file_read",
            json!({"path":"backend/src/agent.js","start_line":i * 5 + 1,"max_lines":5}),
        )
    }));
    let (_, _, offered) = run_scripted_tools(s, steps).await;
    assert!(offered.len() >= 11);
    for (round, names) in offered.iter().take(11).enumerate() {
        for discovery in ["file_list", "source_search"] {
            assert!(
                names.iter().any(|name| name == discovery),
                "{discovery} withheld at request {round} before the document existed"
            );
        }
    }
}

#[test]
fn missing_paths_suggest_similar_project_files_and_directories_list_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(dir.path().join("backend/src/agent.js"), "run();\n").unwrap();
    std::fs::write(dir.path().join("backend/src/server.js"), "listen();\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "# App\n").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            exclude: vec![".env*".into()],
            ..Default::default()
        },
        Config::default(),
    );
    // Same stem, different extension.
    let error = tools::execute(&mut s, "file_read", json!({"path":"backend/agent.py"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("file_not_found"), "{error}");
    assert!(error.contains("backend/src/agent.js"), "{error}");
    // Same file name under a wrong prefix.
    let error = tools::execute(&mut s, "file_read", json!({"path":"llm_agent/README.md"}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("README.md. Copy one exactly"), "{error}");
    // Nothing similar: point to file_list. Excluded files are never offered.
    let error = tools::execute(&mut s, "file_read", json!({"path":"main.py"}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("No project file has this name"), "{error}");
    let error = tools::execute(&mut s, "file_read", json!({"path":"config/.env"}))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("Copy one exactly"), "{error}");
    // A directory read shows what it contains.
    let error = tools::execute(&mut s, "file_read", json!({"path":"backend/src"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("path_is_directory"), "{error}");
    assert!(error.contains("agent.js, server.js"), "{error}");
}

#[test]
fn source_search_accepts_a_directory_as_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(
        dir.path().join("backend/src/server.js"),
        "app.post('/api/chat', chat);\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.js"),
        "app.post('/api/chat', other);\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config::default(),
    );
    let result = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"backend","query":"/api/chat"}),
    )
    .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{result}");
    assert!(
        matches[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("backend/src/server.js")
    );
}

#[test]
fn list_cursor_keeps_its_scope_when_the_glob_is_omitted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    for i in 0..120 {
        std::fs::write(dir.path().join(format!("src/m{i:03}.js")), "x();\n").unwrap();
    }
    std::fs::write(dir.path().join("README.md"), "# App\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config::default(),
    );
    let first = tools::execute(
        &mut s,
        "file_list",
        json!({"mode":"paths","path_glob":"src/**"}),
    )
    .unwrap();
    assert_eq!(first["total_files"], 120);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    // The continuation drops path_glob, as the live model did.
    let second =
        tools::execute(&mut s, "file_list", json!({"mode":"paths","cursor":cursor})).unwrap();
    let rest = second["paths"].as_array().unwrap();
    assert_eq!(rest.len(), 20);
    assert!(
        rest.iter()
            .all(|path| path.as_str().unwrap().starts_with("src/"))
    );
    // A different explicit scope is a real mismatch, explained as such.
    let error = tools::execute(
        &mut s,
        "file_list",
        json!({"mode":"paths","path_glob":"**","cursor":cursor}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("issued for different arguments"), "{error}");
}

#[tokio::test]
async fn repeated_final_answers_on_an_unrepaired_review_close_the_run() {
    let (_dir, mut s) = verified_fixture();
    s.config.source_document_review = true;
    let final_answer = || Completion {
        text: "Saved out.md".into(),
        ..Default::default()
    };
    let steps = vec![
        final_answer(),
        // The document review rejects the result.
        Completion {
            text: r#"{"issues":["Flow: state that the loop runs five times"]}"#.into(),
            ..Default::default()
        },
        final_answer(), // rejected, unchanged (1)
        call("read-1", "file_read", json!({"path":"main.js"})), // forced step
        final_answer(), // rejected, unchanged (2)
        call("read-2", "file_read", json!({"path":"main.js"})), // forced step
        final_answer(), // rejected, unchanged (3) -> closing
        call("read-3", "file_read", json!({"path":"main.js"})), // forced step
        final_answer(), // accepted with reported gaps
    ];
    let (result, guidance, offered) = run_scripted_tools(s, steps).await;
    assert_eq!(
        result.status, "complete_with_gaps",
        "{:?}",
        result.last_error
    );
    assert_eq!(
        result.progress_recovery.closing.as_ref().unwrap().reason,
        "review_unrepaired"
    );
    assert!(
        result
            .completion_gaps
            .iter()
            .any(|gap| gap.contains("loop runs five times")),
        "{:?}",
        result.completion_gaps
    );
    // The forced step after a rejected final offers repair tools only.
    for forced in [3, 5, 7] {
        let names = &offered[forced];
        assert!(
            names.iter().any(|name| name == "document_edit_batch"),
            "{names:?}"
        );
        for trivial in [
            "task_plan",
            "task_state",
            "document_inspect",
            "document_audit",
            "history",
        ] {
            assert!(
                !names.iter().any(|name| name == trivial),
                "{trivial} offered: {names:?}"
            );
        }
    }
    assert!(guidance[8]["closing"]["active"] == true);
}

#[test]
fn a_truncated_checkpoint_id_still_acknowledges_the_checkpoint() {
    use mnemoarc::context::ContextManager;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(64_000),
            context_tokens: 64_000,
            output_tokens: 4_000,
            ..Default::default()
        },
    );
    s.add_user("Work".into());
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2500))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    assert!(ContextManager::prepare(&mut s, budget).unwrap());
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    let ack = |id: &str| json!({"id":id,"progress":"Continue the work","no_save_reason":"Nothing new to save"});
    // Too short to identify anything.
    let error = tools::execute(&mut s, "checkpoint_complete", ack(&id[..6]))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("checkpoint_id_mismatch"), "{error}");
    // The model cut the last characters of the 36-character ID.
    let result = tools::execute(&mut s, "checkpoint_complete", ack(&id[..30])).unwrap();
    assert_eq!(result["acknowledged"], true, "{result}");
}

#[tokio::test]
async fn a_fix_made_after_closing_on_an_unrepaired_review_is_reviewed_again() {
    let (dir, mut s) = verified_fixture();
    s.config.source_document_review = true;
    let final_answer = || Completion {
        text: "Saved out.md".into(),
        ..Default::default()
    };
    let path = dir.path().join("out.md");
    let fixed = std::fs::read_to_string(&path).unwrap().replace(
        "A for loop runs work five times.",
        "A for loop runs work exactly five times.",
    );
    let steps = vec![
        final_answer(),
        Completion {
            text: r#"{"issues":["Flow: say exactly how many times the loop runs"]}"#.into(),
            ..Default::default()
        },
        final_answer(), // rejected, unchanged (1)
        call("read-1", "file_read", json!({"path":"main.js"})),
        final_answer(), // rejected, unchanged (2)
        call("read-2", "file_read", json!({"path":"main.js"})),
        final_answer(), // rejected, unchanged (3) -> closing
        // The forced step finally edits the document.
        call(
            "fix",
            "document_edit",
            json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&path).unwrap()),"text":fixed}),
        ),
        final_answer(), // changed document: reviewed once more
        Completion {
            text: r#"{"issues":[]}"#.into(),
            ..Default::default()
        },
        final_answer(),
    ];
    let (result, _, _) = run_scripted_tools(s, steps).await;
    assert_eq!(
        result.document_review.attempts, 2,
        "{:?}",
        result.last_error
    );
    assert!(document_review::approved(&result));
    assert!(
        !result
            .completion_gaps
            .iter()
            .any(|gap| gap.starts_with("문서 검토")),
        "{:?}",
        result.completion_gaps
    );
}

#[tokio::test]
async fn transient_empty_replies_are_retried_before_any_workflow_is_set() {
    let plain = || {
        let dir = tempfile::tempdir().unwrap();
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
                source_answer_review: false,
                ..Default::default()
            },
        );
        s.add_user("What does this project do?".into());
        (dir, s)
    };
    let (_dir, s) = plain();
    let (result, _) = run_scripted(
        s,
        vec![
            Completion::default(),
            Completion::default(),
            Completion {
                text: "It serves an API.".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    // Persistent empty replies still stop the run instead of looping.
    let (_dir, s) = plain();
    let (result, _) = run_scripted(s, vec![Completion::default(); 3]).await;
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("empty_completion")
    );
}

#[tokio::test]
async fn an_output_limit_truncation_withholds_whole_document_writes() {
    let truncated_then_final = || {
        vec![
            Completion {
                length_limited: true,
                discarded_tool_calls: true,
                ..Default::default()
            },
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ]
    };
    // A document small relative to the output limit cannot be the cause:
    // whole writes stay available.
    let (_small_dir, small) = verified_fixture();
    let (result, _) = run_scripted(small, truncated_then_final()).await;
    assert!(!result.progress_recovery.whole_write_withheld);

    let (dir, mut s) = verified_fixture();
    let filler: String = (1..=600)
        .map(|i| format!("Detail line {i} explains one more step of the flow.\n"))
        .collect();
    let hash = tools::hash(&std::fs::read(dir.path().join("out.md")).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":format!("\n# Details\n{filler}")}),
    )
    .unwrap();
    let (result, _) = run_scripted(
        s,
        vec![
            Completion {
                length_limited: true,
                discarded_tool_calls: true,
                ..Default::default()
            },
            Completion {
                text: "Saved out.md".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert!(result.progress_recovery.whole_write_withheld);
    let mut s = result;
    // The schema no longer offers write, and a write call is refused.
    let edit = ToolRegistry::definitions(&s)
        .into_iter()
        .find(|tool| tool["function"]["name"] == "document_edit")
        .unwrap();
    let actions = edit["function"]["parameters"]["properties"]["action"]["enum"].clone();
    assert!(
        !actions.as_array().unwrap().iter().any(|a| a == "write"),
        "{actions}"
    );
    assert!(actions.as_array().unwrap().iter().any(|a| a == "section"));
    let path = dir.path().join("out.md");
    let hash = tools::hash(&std::fs::read(&path).unwrap());
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\\nRewritten.\\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("whole_write_withheld"), "{error}");
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"write","text":"# Flow\\nRewritten.\\n"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("whole_write_withheld"), "{error}");
    // A section-sized edit succeeds and restores whole writes.
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"History is normalized first.","text":"History is normalized before the loop."}),
    )
    .unwrap();
    assert!(!s.progress_recovery.whole_write_withheld);
}

#[test]
fn repair_reverification_counts_as_fresh_progress() {
    let (_dir, mut s, source) = fixture();
    let verify = |s: &mut Session, id: &str| {
        tools::execute(s, "investigation", json!({"action":"verify","id":id,"source_ids":[source],"verification_note":"Compared the cited lines with the section"})).unwrap();
    };
    verify(&mut s, "flow");
    assert_eq!(s.progress_recovery.verification_events, 1);
    // Re-verifying an unchanged, already verified item is not new work.
    verify(&mut s, "flow");
    assert_eq!(s.progress_recovery.verification_events, 1);
    // A repair edit invalidates the section; verifying it again counts.
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"A for loop runs work five times.","text":"A for loop runs work exactly five times."}),
    )
    .unwrap();
    assert_ne!(
        s.investigations
            .iter()
            .find(|i| i.id == "flow")
            .unwrap()
            .status,
        "verified"
    );
    verify(&mut s, "flow");
    assert_eq!(s.progress_recovery.verification_events, 2);
}

#[test]
fn paged_rereview_judges_previous_findings_only_on_their_page() {
    let (_dir, mut s, _) = fixture();
    let request = document_review::request(&mut s).unwrap();
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("List ONLY problems that are still present"));
    assert!(system.contains("skip it otherwise"));
    assert!(system.contains("cannot be observed on this page"));
}

#[tokio::test]
async fn a_checkpoint_during_review_repair_restates_the_open_findings() {
    use mnemoarc::context::ContextManager;
    let (_dir, mut s) = verified_fixture();
    s.config.source_document_review = true;
    let final_answer = || Completion {
        text: "Saved out.md".into(),
        ..Default::default()
    };
    // The review rejects the document, which then stays unchanged.
    let (mut s, _, _) = run_scripted_tools(
        s,
        vec![
            final_answer(),
            Completion {
                text: r#"{"issues":["Flow: state that the loop runs five times"]}"#.into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert!(document_review::rejected_on_current_result(&s));
    // A checkpoint then clears the context that held the findings.
    for i in 0..6 {
        s.history.push(
            vec![json!({"role":"user","content":format!("{i} {}", "context ".repeat(2500))})],
            true,
        );
    }
    let budget = ContextManager::input_budget(&s.config);
    assert!(ContextManager::prepare(&mut s, budget).unwrap());
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    tools::execute(
        &mut s,
        "checkpoint_complete",
        json!({"id":id,"progress":"Repairing the review findings","no_save_reason":"Nothing new to save"}),
    )
    .unwrap();
    ContextManager::commit(&mut s).unwrap();
    assert!(s.progress_recovery.review_repair_resume_hash.is_some());
    let (after, guidance, _) = run_scripted_tools(s, vec![final_answer()]).await;
    assert!(
        !guidance.is_empty(),
        "{} {:?}",
        after.status,
        after.last_error
    );
    let repair = &guidance[0]["review_repair"];
    assert_eq!(repair["resumed_after_checkpoint"], true, "{}", guidance[0]);
    assert!(
        repair["unrepaired_findings"][0]
            .as_str()
            .unwrap()
            .contains("loop runs five times")
    );
    assert!(
        guidance[0]["instruction"]
            .as_str()
            .unwrap()
            .starts_with("A checkpoint cleared the context")
    );
}

#[tokio::test]
async fn a_forced_repair_step_refuses_non_repair_tools() {
    let (_dir, mut s) = verified_fixture();
    s.config.source_document_review = true;
    let final_answer = || Completion {
        text: "Saved out.md".into(),
        ..Default::default()
    };
    let steps = vec![
        final_answer(),
        Completion {
            text: r#"{"issues":["Flow: state that the loop runs five times"]}"#.into(),
            ..Default::default()
        },
        final_answer(), // rejected, unchanged
        // The live shape: an audit instead of an edit in the forced step.
        call("audit", "document_audit", json!({})),
        final_answer(),
        call("read-1", "file_read", json!({"path":"main.js"})),
        final_answer(), // third unchanged rejection -> closing
        call("read-2", "file_read", json!({"path":"main.js"})),
        final_answer(),
    ];
    let (result, _, offered) = run_scripted_tools(s, steps).await;
    let audit = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .find(|message| message["tool_call_id"] == "audit")
        .expect("audit result");
    assert!(
        audit["content"]
            .as_str()
            .unwrap()
            .contains("review_repair_required:"),
        "{audit}"
    );
    // Re-verifying the unchanged document is not offered as repair.
    assert!(!offered[3].iter().any(|name| name == "investigation"));
    assert!(offered[3].iter().any(|name| name == "document_edit_batch"));
}
