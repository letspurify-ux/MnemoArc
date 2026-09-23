use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{RunCommand, run_session, run_session_controlled},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
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
fn s(dir: &std::path::Path) -> Session {
    Session::new(
        Project {
            root: dir.into(),
            ..Default::default()
        },
        Config {
            source_answer_review: false,
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        },
    )
}
struct Script {
    step: Mutex<usize>,
}
#[async_trait]
impl LlmClient for Script {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if request["messages"][1]["content"]
            .as_str()
            .is_some_and(|t| t.contains("\"source_document_review\":true"))
        {
            assert!(request.get("tools").is_none());
            return Ok(Completion {
                text: r#"{"issues":[]}"#.into(),
                ..Default::default()
            });
        }
        let mut step = self.step.lock().unwrap();
        let result = match *step {
            0 => call(
                "select",
                "tool_select",
                json!({"action":"add","names":["source-docs"]}),
            ),
            1 => {
                assert!(
                    request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["function"]["name"] == "file_read")
                );
                call("read", "file_read", json!({"path":"main.rs"}))
            }
            2 => {
                let messages = request["messages"].as_array().unwrap();
                let read: Value = serde_json::from_str(
                    messages.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                call(
                    "remember",
                    "memory_write",
                    json!({"key":"entry","title":"Entry","summary":"Main prints hello","body":"The main function prints hello once.","kind":"fact","source_ids":[read["data"]["source"]["id"]]}),
                )
            }
            3 => call(
                "draft",
                "document_edit",
                json!({"action":"create","text":"# Entry\nThe main function prints hello once. [Source](../main.rs#L1)\n"}),
            ),
            4 => {
                let state: Value = serde_json::from_str(
                    request["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .split_once('\n')
                        .unwrap()
                        .1,
                )
                .unwrap();
                let memory_id = state["recent_memories"][0]["id"].clone();
                let source = source_id(&request);
                call(
                    "item",
                    "investigation",
                    json!({"action":"upsert","id":"entry-item","title":"Entry point","status":"written","memory_ids":[memory_id],"source_ids":[source],"section":"# Entry"}),
                )
            }
            5 => call(
                "verify",
                "investigation",
                json!({"action":"verify","id":"entry-item","source_ids":[source_id(&request)],"verification_note":"Compared main.rs line 1 with the generated Entry section; both state a single print"}),
            ),
            6 => call(
                "final-check",
                "investigation",
                json!({"action":"final_check"}),
            ),
            _ => Completion {
                text: "Created docs/source-summary.md. Entry point verified; no unknowns.".into(),
                ..Default::default()
            },
        };
        *step += 1;
        Ok(result)
    }
}
fn source_id(request: &Value) -> Value {
    let result: Value = serde_json::from_str(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["tool_call_id"] == "read")
            .unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    result["data"]["source"]["id"].clone()
}
#[tokio::test]
async fn source_documentation_full_loop_without_api() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();
    let mut session = s(dir.path());
    session.add_user("Document this project's entry point with source evidence".into());
    let (tx, mut rx) = mpsc::channel(128);
    let reader = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(Script {
            step: Mutex::new(0),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    reader.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.memory.entries.len(), 1);
    assert_eq!(result.investigations[0].status, "verified");
    assert!(dir.path().join("docs/source-summary.md").exists());
    assert!(!dir.path().join("config.toml").exists());
    assert_eq!(result.reviews, 1);
}
struct Wait;
#[async_trait]
impl LlmClient for Wait {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> Result<Completion> {
        delta.send("partial".into()).await.ok();
        cancel.cancelled().await;
        anyhow::bail!("cancelled")
    }
}
#[tokio::test]
async fn cancellation_keeps_goal_and_user_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.task.constraints.push("Preserve sources".into());
    session.add_user("Please investigate".into());
    let token = CancellationToken::new();
    let cancel = token.clone();
    let (tx, mut rx) = mpsc::channel(16);
    let reader = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, mnemoarc::agent::AgentEvent::Delta { .. }) {
                cancel.cancel();
            }
        }
    });
    let result = run_session(session, Arc::new(Wait), token, tx).await;
    reader.await.unwrap();
    assert_eq!(result.status, "cancelled");
    assert_eq!(result.task.constraints, ["Preserve sources"]);
    assert_eq!(result.history.active().len(), 1);
    assert!(result.usage_incomplete);
}
struct ConfigureDuringCall {
    step: Mutex<usize>,
    tx: mpsc::Sender<RunCommand>,
    config: Config,
}
#[async_trait]
impl LlmClient for ConfigureDuringCall {
    async fn complete(
        &self,
        _: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let step = {
            let mut i = self.step.lock().unwrap();
            let step = *i;
            *i += 1;
            step
        };
        if step == 0 {
            assert_eq!(config.output_tokens, 8000);
            self.tx
                .send(RunCommand::Configure(Box::new(self.config.clone())))
                .await
                .unwrap();
            assert_eq!(config.output_tokens, 8000);
            Ok(call(
                "goals",
                "task_state",
                json!({"action":"update","patch":{"findings":["compare documents"]}}),
            ))
        } else {
            assert_eq!(config.output_tokens, 4000);
            Ok(Completion {
                text: "Comparison complete".into(),
                ..Default::default()
            })
        }
    }
}
#[tokio::test]
async fn settings_apply_at_next_request_and_generic_tasks_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.add_user("Compare two proposed plans".into());
    let mut config = session.config.clone();
    config.output_tokens = 4000;
    let (tx, rx) = mpsc::channel(4);
    let client = ConfigureDuringCall {
        step: Mutex::new(0),
        tx,
        config,
    };
    let (events, mut events_rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
    let result = run_session_controlled(
        session,
        Arc::new(client),
        CancellationToken::new(),
        events,
        rx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.config.output_tokens, 4000);
    assert_eq!(result.task.findings, ["compare documents"]);
    assert!(result.active_tools.contains("file_read"));
    assert!(!result.active_tools.contains("document_edit"));
    assert_eq!(result.status, "complete");
}

struct RetryCleanup {
    step: Mutex<usize>,
    key: String,
}
#[async_trait]
impl LlmClient for RetryCleanup {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        assert!(
            mnemoarc::context::count(&request, &config.model) + config.output_tokens + 512
                <= config.context_tokens
        );
        let response = match *step {
            0 => {
                assert_eq!(config.output_tokens, 8000);
                assert!(
                    request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|t| t["function"]["name"] != "document_edit")
                );
                call(
                    &format!("memory-{}", self.key),
                    "memory_write",
                    json!({"key":self.key,"title":"너무 긴 제목".repeat(150),"summary":"과도한 메타데이터","body":"보존할 근거와 실패 기록".repeat(150),"kind":"fact"}),
                )
            }
            1 => {
                assert!(!request.to_string().contains("memory_metadata_limit"));
                Completion {
                    calls: vec![
                        ToolCall{id:format!("ack-{}",state["checkpoint"]["id"]),name:"checkpoint_complete".into(),arguments:json!({"id":state["checkpoint"]["id"],"progress":"필요한 기억을 보존했고 원본 수정 금지를 유지한다. 다음은 문서 작성이다."}).to_string()},
                        ToolCall{id:format!("progress-{}",state["checkpoint"]["id"]),name:"task_state".into(),arguments:json!({"action":"update","patch":{"findings":["실패 근거와 다음 작업 저장 완료"]}}).to_string()},
                    ], ..Default::default()
                }
            }
            2 => {
                assert!(state["checkpoint"].is_null());
                assert_eq!(
                    state["task"]["constraints"][0],
                    "원본 소스는 변경하지 않는다"
                );
                Completion {
                    text: "정리 후 작업을 계속할 수 있습니다.".into(),
                    ..Default::default()
                }
            }
            _ => panic!("unexpected extra model request"),
        };
        *step += 1;
        Ok(response)
    }
}

#[tokio::test]
async fn unknown_model_accepts_large_memory_metadata_over_three_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.config.model = "z-ai/glm-5.3-flash".into();
    session.config.index_tokens = 20_000;
    session
        .task
        .constraints
        .push("원본 소스는 변경하지 않는다".into());
    for cycle in 0..3 {
        for _ in 0..4 {
            session.history.push(
                vec![json!({"role":"user","content":"확인한 소스 근거와 처리 흐름. ".repeat(120)})],
                true,
            );
        }
        mnemoarc::context::ContextManager::prepare(&mut session, 60000).unwrap();
        let client = Arc::new(RetryCleanup {
            step: Mutex::new(0),
            key: format!("large-{cycle}"),
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        session = run_session(session, client, CancellationToken::new(), tx).await;
        drain.await.unwrap();
        assert_eq!(session.status, "complete", "{:?}", session.last_error);
        assert_eq!(session.checkpoints_completed, cycle + 1);
        assert_eq!(session.memory.entries.len(), cycle + 1);
        for message in session.history.bundles.iter().flat_map(|b| &b.messages) {
            if message["role"] != "tool" {
                continue;
            }
            let id = message["tool_call_id"].as_str().unwrap();
            let result: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
            if id.starts_with("ack-") {
                assert_eq!(result["data"]["acknowledged"], true);
            }
        }
        assert!(
            session
                .history
                .active()
                .iter()
                .all(|m| m["tool_calls"].is_null()),
            "maintenance exchanges must not trigger another cleanup"
        );
        assert!(
            session
                .history
                .bundles
                .iter()
                .any(|b| !b.active && b.reviewed)
        );
    }
    assert!(session.memory.search("실패", &[]).len() >= 3);
}

struct InvalidCleanup;
#[async_trait]
impl LlmClient for InvalidCleanup {
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
        let attempt = state["checkpoint"]["attempts"].as_u64().unwrap();
        Ok(Completion {
            calls: vec![
                ToolCall {
                    id: format!("ack-{attempt}"),
                    name: "checkpoint_complete".into(),
                    arguments:
                        json!({"id":state["checkpoint"]["id"],"progress":"Progress must not change on failed save","no_save_reason":"Already preserved"})
                            .to_string(),
                },
                ToolCall {
                    id: format!("bad-{attempt}"),
                    name: "memory_write".into(),
                    arguments: json!({"unknown_field":"invalid"}).to_string(),
                },
            ],
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn early_ack_cannot_hide_failed_saves_in_the_same_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.config.stall_round_limit = 9;
    session.add_user("Preserve this original constraint".into());
    session.add_user("Next observation".into());
    mnemoarc::context::ContextManager::prepare(&mut session, 60000).unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(InvalidCleanup),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("checkpoint_retry_limit")),
        "{:?}",
        result.last_error
    );
    assert_eq!(result.checkpoints_completed, 0);
    assert!(result.history.bundles.iter().all(|b| b.active));
    let cp = result.checkpoint.unwrap();
    assert_eq!(cp.attempts, 8);
    assert_eq!(cp.failed_attempts, 8);
    assert!(
        !cp.last_failure
            .unwrap()
            .contains("checkpoint_has_failed_operations")
    );
}

struct NeverCalled;
#[async_trait]
impl LlmClient for NeverCalled {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        panic!("Oversized requests must not reach the provider")
    }
}
#[tokio::test]
async fn input_overflow_does_not_spend_cleanup_attempts_or_remove_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.history.push(
        vec![json!({"role":"user","content":"evidence ".repeat(70000)})],
        true,
    );
    session
        .history
        .push(vec![json!({"role":"user","content":"Keep going"})], true);
    let before = session.history.active();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(session, Arc::new(NeverCalled), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert!(
        result
            .last_error
            .unwrap()
            .contains("context_limit: input estimate")
    );
    assert_eq!(result.checkpoint.unwrap().attempts, 0);
    assert_eq!(result.history.active(), before);
}

struct RepeatedRead {
    step: Mutex<usize>,
}
#[async_trait]
impl LlmClient for RepeatedRead {
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
        )
        .unwrap();
        if state["run_guidance"]["progress_recovery"]["active"] == true {
            return Ok(Completion {
                text: "The source was already read; proceeding with the answer.".into(),
                ..Default::default()
            });
        }
        let mut step = self.step.lock().unwrap();
        *step += 1;
        Ok(call(
            &format!("repeat-{step}"),
            "file_read",
            json!({"path":"main.rs"}),
        ))
    }
}
#[tokio::test]
async fn unchanged_parallel_reads_are_suppressed_then_answer_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut session = s(dir.path());
    session.active_tools = mnemoarc::tools::ToolRegistry::optional_names();
    session.config.stall_round_limit = 4;
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(RepeatedRead {
            step: Mutex::new(0),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(
        result
            .history
            .bundles
            .iter()
            .flat_map(|b| &b.messages)
            .any(|m| m["content"]
                .as_str()
                .is_some_and(|t| t.contains("\"suppressed\":true")))
    );
    assert_eq!(result.history.bundles.len(), 5);
}

struct VariedReadsThenWrites(Mutex<usize>);
#[async_trait]
impl LlmClient for VariedReadsThenWrites {
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
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        let response = if *step < 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], false);
            call(
                &format!("read-{step}"),
                "file_read",
                json!({"path":"main.rs","start_line":*step+1,"max_lines":1}),
            )
        } else if *step == 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
            assert_eq!(state["run_guidance"]["phase"], "draft");
            let tools = request["tools"].as_array().unwrap();
            assert!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "document_edit")
            );
            assert!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "file_read")
            );
            assert!(
                !tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "source_search")
            );
            call(
                "write",
                "document_edit",
                json!({"action":"create","text":"# Summary\nThe requested summary is saved.\n"}),
            )
        } else {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], false);
            Completion {
                text: "Saved the summary.".into(),
                ..Default::default()
            }
        };
        *step += 1;
        Ok(response)
    }
}

#[tokio::test]
async fn varied_reads_without_deliverable_progress_focus_on_writing_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "one\ntwo\nthree\n").unwrap();
    let mut session = s(dir.path());
    session.config.stall_round_limit = 3;
    session.task.workflow = "document_edit".into();
    session.task.deliverables = vec!["docs/source-summary.md".into()];
    session.active_tools.insert("document_edit".into());
    session.add_user("Save a summary of the source".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(VariedReadsThenWrites(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.task_rounds, 5);
    assert!(dir.path().join("docs/source-summary.md").exists());
}

#[tokio::test]
async fn resumed_document_work_retains_no_progress_count() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "one\ntwo\nthree\n").unwrap();
    let mut session = s(dir.path());
    session.config.stall_round_limit = 3;
    session.task.workflow = "document_edit".into();
    session.task.deliverables = vec!["docs/source-summary.md".into()];
    session.active_tools.insert("document_edit".into());
    session.add_user("Save a summary of the source".into());
    session.run_guidance =
        json!({"progress_recovery":{"rounds_without_progress":2,"repeated_read":false}});
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(VariedReadsThenWrites(Mutex::new(2))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.task_rounds, 3);
}

struct BudgetPhases {
    step: Mutex<usize>,
}
#[async_trait]
impl LlmClient for BudgetPhases {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(
            state["run_guidance"]["phase"],
            ["investigate", "draft", "verify"][*step]
        );
        let result = if *step == 2 {
            Completion {
                text: "Partial result; evidence still needs review".into(),
                ..Default::default()
            }
        } else {
            let mut c = call(
                &format!("state-{step}"),
                "task_state",
                json!({"action":"read"}),
            );
            c.usage = Some(mnemoarc::llm::Usage {
                input: if *step == 0 { 260000 } else { 125000 },
                output: 0,
                cached: None,
            });
            c
        };
        *step += 1;
        Ok(result)
    }
}
#[tokio::test]
async fn request_budget_transitions_to_writing_then_verification() {
    let dir = tempfile::tempdir().unwrap();
    let session = s(dir.path());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(BudgetPhases {
            step: Mutex::new(0),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete");
    assert_eq!(result.run_guidance["phase"], "verify");
}

struct PrematureFinal {
    calls: Mutex<usize>,
}
#[async_trait]
impl LlmClient for PrematureFinal {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut calls = self.calls.lock().unwrap();
        if *calls > 0 {
            let state = request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            let state: Value = serde_json::from_str(state.split_once('\n').unwrap().1).unwrap();
            assert!(
                !state["run_guidance"]["completion_error"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
        }
        *calls += 1;
        Ok(Completion {
            text: "Everything is complete".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn premature_final_is_retried_but_never_claimed_complete_without_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.document_written = true;
    session.task.require_investigation = true;
    let client = Arc::new(PrematureFinal {
        calls: Mutex::new(0),
    });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(session, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.status, "partial");
    assert_eq!(*client.calls.lock().unwrap(), 9);
    assert_eq!(result.run_guidance["phase"], "verify");
    // Rejected completion claims must not appear as final answers in UI history.
    assert!(result.history.bundles.is_empty());
}

struct SummaryReads {
    step: Mutex<usize>,
    offsets: Mutex<Vec<usize>>,
    text: String,
}
#[async_trait]
impl LlmClient for SummaryReads {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        let mut offsets = self.offsets.lock().unwrap();
        let mut calls = vec![];
        if *step == 0 {
            assert!(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "file_read")
            );
            for i in 0..4 {
                calls.push(ToolCall {
                    id: format!("read-{i}"),
                    name: "file_read".into(),
                    arguments: json!({"path":format!("part-{i}.md"),"start_line":1,"max_lines":1})
                        .to_string(),
                });
            }
        } else {
            let results: Vec<Value> = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .rev()
                .take(4)
                .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            assert_eq!(results.len(), 4);
            let mut total = 0;
            for (i, result) in results.iter().enumerate() {
                let numbered = result["data"]["content"]["numbered_text"]
                    .as_str()
                    .expect("every read must retain actual document text");
                // This fixture is one long line; labels never participate in
                // source cursor offsets or the stored coverage.
                let text = numbered.strip_prefix("1|").unwrap();
                assert!(text.chars().count() > 500, "later reads must not starve");
                let expected: String = self
                    .text
                    .chars()
                    .skip(offsets[i])
                    .take(text.chars().count())
                    .collect();
                assert!(
                    text == expected,
                    "step={} part={i} expected_offset={} read_offset={} first={:?} expected={:?}",
                    *step,
                    offsets[i],
                    result["data"]["read_offset"],
                    text.chars().take(30).collect::<String>(),
                    expected.chars().take(30).collect::<String>()
                );
                offsets[i] += text.chars().count();
                let cursor = &result["next_cursor"];
                assert_eq!(cursor["tool"], "file_read");
                assert_eq!(
                    result["data"]["next_offset"].as_u64().unwrap() as usize,
                    offsets[i]
                );
                assert!(cursor["cursor"].as_str().unwrap().starts_with('R'));
                assert!(cursor.get("offset").is_none());
                total += mnemoarc::tools::result_tokens(
                    &ToolCall {
                        id: format!("read-{i}"),
                        name: "file_read".into(),
                        arguments: "{}".into(),
                    },
                    result,
                    "gpt-4o",
                );
                let mut args = cursor.clone();
                args.as_object_mut().unwrap().remove("tool");
                calls.push(ToolCall {
                    id: format!("continue-{i}"),
                    name: "file_read".into(),
                    arguments: args.to_string(),
                });
            }
            assert!(total <= 8000);
        }
        *step += 1;
        if *step == 3 {
            Ok(Completion {
                text: "```mermaid\nflowchart LR\n A --> B\n```".into(),
                ..Default::default()
            })
        } else {
            Ok(Completion {
                calls,
                ..Default::default()
            })
        }
    }
}
#[tokio::test]
async fn summary_reads_share_budget_and_continue_without_history_or_writes() {
    let dir = tempfile::tempdir().unwrap();
    let text = "백엔드 요청 처리와 검색 결과 설명. ".repeat(2000);
    for i in 0..4 {
        std::fs::write(dir.path().join(format!("part-{i}.md")), &text).unwrap();
    }
    let mut session = s(dir.path());
    session.config.context_tokens = 128000;
    session.config.result_tokens = 4000;
    session.config.batch_tokens = 8000;
    session.add_user("기존 문서에서 backend 부분만 mermaid로 요약해줘".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(SummaryReads {
            step: Mutex::new(0),
            offsets: Mutex::new(vec![0; 4]),
            text: text.clone(),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    for bundle in &result.history.bundles {
        for message in &bundle.messages {
            if message["role"] != "tool" {
                continue;
            }
            let output: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
            if let Some(text) = output["data"]["content"]["text"].as_str() {
                let start = output["data"]["content"]["line_start"].as_u64().unwrap() as usize;
                let end = start + text.lines().count() - 1;
                let id = output["data"]["source"]["id"].as_str().unwrap();
                assert_eq!(output["data"]["source"]["end_line"], end);
                assert_eq!(result.sources[id].end_line, Some(end));
                assert_eq!(
                    result.sources[id].excerpt,
                    text.chars().take(2000).collect::<String>()
                );
            }
        }
    }
    // Parallel worker results are recorded only after final batch truncation.
    assert_eq!(result.read_coverage.len(), 4);
    for coverage in result.read_coverage.values() {
        assert_eq!(coverage.ranges.len(), 1);
        assert_eq!(coverage.ranges[0].0, 0);
        assert!(coverage.ranges[0].1 > 0 && coverage.ranges[0].1 < text.chars().count());
    }
    assert!(result.investigations.is_empty());
    assert!(result.memory.entries.is_empty());
    assert!(!dir.path().join("docs/source-summary.md").exists());
    for i in 0..4 {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(format!("part-{i}.md"))).unwrap(),
            text
        );
    }
}

struct ExpectPhase(&'static str);
#[async_trait]
impl LlmClient for ExpectPhase {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1).unwrap();
        assert_eq!(state["run_guidance"]["phase"], self.0);
        Ok(Completion {
            text: "Answer from available evidence.".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn resume_and_large_budgets_do_not_restart_investigation() {
    for (phase, rounds, expected) in [("draft", 2, "draft"), ("", 6, "answer")] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = s(dir.path());
        session.config.run_tokens = 5_000_000;
        session.task.phase = phase.into();
        session.task_rounds = rounds;
        session.add_user("계속 진행".into());
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move {
            let mut model = false;
            while let Some(event) = rx.recv().await {
                if let mnemoarc::agent::AgentEvent::Snapshot(s) = event
                    && s.activity["stage"] == "model"
                {
                    model = true;
                    assert!(s.activity["started_at_ms"].as_i64().unwrap() > 0);
                }
            }
            assert!(model);
        });
        let result = run_session(
            session,
            Arc::new(ExpectPhase(expected)),
            CancellationToken::new(),
            tx,
        )
        .await;
        drain.await.unwrap();
        assert_eq!(result.status, "complete");
        assert_eq!(result.activity["stage"], "idle");
    }
}

struct LengthScript {
    step: Mutex<usize>,
    always_empty: bool,
    tools: bool,
}
#[async_trait]
impl LlmClient for LengthScript {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        if *step > 0 {
            assert!(
                request["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("LENGTH RECOVERY")
            );
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|m| m.get("partial").is_none() && m.get("continues_previous").is_none())
            );
            if self.tools {
                assert!(
                    request["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("NONE of those calls executed")
                );
            }
            if !self.always_empty {
                assert!(
                    request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["content"] == "```mermaid\nflowchart LR\n A -->")
                );
            }
        }
        let limited = *step == 0 || self.always_empty;
        *step += 1;
        Ok(Completion {
            text: if self.always_empty {
                "".into()
            } else if limited {
                "```mermaid\nflowchart LR\n A -->".into()
            } else {
                " B\n```".into()
            },
            length_limited: limited,
            discarded_tool_calls: limited && self.tools,
            usage: Some(mnemoarc::llm::Usage {
                input: 100,
                output: 10,
                cached: None,
            }),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn length_continues_text_and_bounds_empty_reasoning_loops() {
    for (empty, tools) in [(false, false), (false, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = s(dir.path());
        session.add_user("Draw a diagram".into());
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(
            session,
            Arc::new(LengthScript {
                step: Mutex::new(0),
                always_empty: empty,
                tools,
            }),
            CancellationToken::new(),
            tx,
        )
        .await;
        drain.await.unwrap();
        assert!(!result.document_written);
        if empty {
            assert_eq!(result.status, "blocked");
            assert!(
                result
                    .last_error
                    .unwrap()
                    .starts_with("length_recovery_limit")
            );
            assert_eq!(result.task_rounds, 8);
            assert!(result.continuation.is_some());
        } else {
            assert_eq!(result.status, "complete");
            assert_eq!((result.input_tokens, result.output_tokens), (200, 20));
            let messages = result.history.active();
            assert_eq!(messages[1]["partial"], true);
            assert_eq!(messages[2]["continues_previous"], true);
            assert_eq!(
                format!(
                    "{}{}",
                    messages[1]["content"].as_str().unwrap(),
                    messages[2]["content"].as_str().unwrap()
                ),
                "```mermaid\nflowchart LR\n A --> B\n```"
            );
            assert!(result.continuation.is_none());
        }
    }
}

struct ExhaustOnLength;
#[async_trait]
impl LlmClient for ExhaustOnLength {
    async fn complete(
        &self,
        _: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        Ok(Completion {
            text: "Retained prefix".into(),
            length_limited: true,
            usage: Some(mnemoarc::llm::Usage {
                input: config.run_tokens,
                output: 1,
                cached: None,
            }),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn length_continuation_obeys_run_budget_and_preserves_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.add_user("Answer".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(ExhaustOnLength),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.task_rounds, 1);
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .unwrap()
            .starts_with("run_budget_exhausted")
    );
    assert!(
        result
            .history
            .active()
            .iter()
            .any(|m| m["content"] == "Retained prefix" && m["partial"] == true)
    );
    assert!(result.continuation.is_some());
}

#[tokio::test]
async fn simple_summary_append_completes_without_investigation_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.active_tools.insert("document_edit".into());
    session.active_tools.insert("investigation".into());
    session.add_user("결과 파일에 전체 summary도 추가해줘".into());
    let original = "# Backend\nExisting description.\n";
    let output = dir.path().join("summary.md");
    session.project.output = output.clone();
    std::fs::write(&output, original).unwrap();
    mnemoarc::tools::execute(
        &mut session,
        "document_edit",
        json!({
            "action":"append", "text":"\n## Summary\nProject overview.\n",
            "expected_hash":mnemoarc::tools::hash(original.as_bytes())
        }),
    )
    .unwrap();
    let client = Arc::new(PrematureFinal {
        calls: Mutex::new(0),
    });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(session, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.last_error.is_none());
    assert!(result.investigations.is_empty());
    assert_eq!(*client.calls.lock().unwrap(), 1);
    assert!(
        std::fs::read_to_string(output)
            .unwrap()
            .contains("## Summary")
    );
}

#[tokio::test]
async fn simple_edit_cannot_complete_if_saved_file_changes_or_disappears() {
    for remove in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = s(dir.path());
        session.active_tools.insert("document_edit".into());
        session.active_tools.insert("investigation".into());
        session.project.output = dir.path().join("summary.md");
        mnemoarc::tools::execute(
            &mut session,
            "document_edit",
            json!({
                "action":"create", "text":"# Summary\nSaved content.\n"
            }),
        )
        .unwrap();
        if remove {
            std::fs::remove_file(&session.project.output).unwrap();
        } else {
            std::fs::write(&session.project.output, "External changes").unwrap();
        }
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(
            session,
            Arc::new(PrematureFinal {
                calls: Mutex::new(0),
            }),
            CancellationToken::new(),
            tx,
        )
        .await;
        drain.await.unwrap();
        assert_eq!(result.status, "partial");
        assert!(result.last_error.unwrap().contains(if remove {
            "document_write_verification_failed"
        } else {
            "document_changed_after_write"
        }));
    }
}

#[test]
fn evidence_requirement_is_explicit_and_retained_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.active_tools.insert("document_edit".into());
    session.active_tools.insert("investigation".into());
    mnemoarc::tools::execute(
        &mut session,
        "task_state",
        json!({
            "action":"update", "patch":{"require_investigation":true}
        }),
    )
    .unwrap();
    let error = mnemoarc::tools::execute(
        &mut session,
        "task_state",
        json!({
            "action":"update", "patch":{"require_investigation":false}
        }),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("investigation_requirement_locked")
    );
    session.add_user("계속 진행".into());
    assert!(session.task.require_investigation);
    session.add_user("문서 제목만 바꿔줘".into());
    assert!(!session.task.require_investigation);
    assert!(!session.document_written);
    let result = mnemoarc::tools::execute(
        &mut session,
        "investigation",
        json!({"action":"final_check"}),
    )
    .unwrap();
    assert_eq!(result["complete"], false);
    assert!(session.task.require_investigation);
}

#[tokio::test]
async fn existing_unverified_investigation_still_blocks_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.active_tools.insert("document_edit".into());
    session.active_tools.insert("investigation".into());
    mnemoarc::tools::execute(
        &mut session,
        "investigation",
        json!({
            "action":"upsert", "id":"pending", "title":"Source analysis"
        }),
    )
    .unwrap();
    assert!(!session.task.require_investigation);
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(PrematureFinal {
            calls: Mutex::new(0),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "partial");
    assert!(
        result
            .last_error
            .unwrap()
            .contains("Unverified investigation")
    );
}

struct RecoverUnknownSource {
    observed: String,
}
#[async_trait]
impl LlmClient for RecoverUnknownSource {
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
        let cp = &state["checkpoint"];
        if cp.is_null() {
            return Ok(Completion {
                text: "Recovered".into(),
                ..Default::default()
            });
        }
        let round = cp["attempts"].as_u64().unwrap();
        if round == 3 {
            assert_eq!(cp["failed_attempts"], 1);
            assert!(
                cp["last_failure"]
                    .as_str()
                    .unwrap()
                    .contains("unknown_source: S93-missing")
            );
            return Ok(call(
                "lookup",
                "source_lookup",
                json!({"path":"evidence.rs"}),
            ));
        }
        let source = if round == 2 {
            "S93-missing"
        } else {
            &self.observed
        };
        let mut response = call(
            &format!("save-{round}"),
            "memory_write",
            json!({
                "title":format!("Finding {round}"),"summary":"Observed fact","kind":"fact",
                "body":"Entry returns successfully.","source_ids":[source]
            }),
        );
        if round == 4 {
            let messages = request["messages"].as_array().unwrap();
            assert!(messages.iter().any(|m| {
                m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains(&self.observed) && c.contains("items"))
            }));
            response.calls.push(ToolCall {
                id: "ack".into(), name: "checkpoint_complete".into(),
                arguments: json!({"id":cp["id"],"progress":"Saved verified findings", "next":"Resume investigation"}).to_string(),
            });
        }
        assert!(round <= 4);
        Ok(response)
    }
}
#[tokio::test]
async fn successful_saves_then_unknown_source_can_lookup_repair_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("evidence.rs"), "fn entry() {}\n").unwrap();
    let mut session = s(dir.path());
    let evidence = mnemoarc::tools::execute(
        &mut session,
        "file_read",
        json!({"path":"evidence.rs","start_line":1,"max_lines":1}),
    )
    .unwrap();
    let observed = evidence["source"]["id"].as_str().unwrap().to_string();
    session.add_user("Preserve original evidence".into());
    mnemoarc::context::ContextManager::prepare(&mut session, 60000).unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(RecoverUnknownSource { observed }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.checkpoints_completed, 1);
    assert_eq!(result.memory.entries.len(), 3);
    assert!(result.checkpoint.is_none());
    assert!(result.history.bundles.iter().any(|b| {
        !b.active
            && b.messages
                .iter()
                .any(|m| m["content"] == "Preserve original evidence")
    }));
}

struct NeverAcknowledges;
#[async_trait]
impl LlmClient for NeverAcknowledges {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        Ok(call(
            "lookup",
            "source_lookup",
            json!({"path":"unobserved.rs"}),
        ))
    }
}
#[tokio::test]
async fn successful_maintenance_without_ack_is_still_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.add_user("Keep original".into());
    mnemoarc::context::ContextManager::prepare(&mut session, 60000).unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(NeverAcknowledges),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "blocked");
    let cp = result.checkpoint.unwrap();
    assert_eq!(cp.attempts, 8);
    assert_eq!(cp.failed_attempts, 0);
    assert!(
        result
            .last_error
            .unwrap()
            .contains("checkpoint_complete was not called")
    );
    assert!(result.history.bundles.iter().all(|b| b.active));
}

struct SeparateLengthRecoveries(Mutex<usize>);
#[async_trait]
impl LlmClient for SeparateLengthRecoveries {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut round = self.0.lock().unwrap();
        *round += 1;
        if *round <= 6 && *round % 2 == 1 {
            return Ok(Completion {
                length_limited: true,
                discarded_tool_calls: true,
                ..Default::default()
            });
        }
        if *round <= 6 {
            return Ok(call(
                &format!("read-{round}"),
                "task_state",
                json!({"action":"read"}),
            ));
        }
        Ok(Completion {
            text: "Done".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn independent_output_truncations_do_not_exhaust_each_others_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.add_user("Work in stages".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        session,
        Arc::new(SeparateLengthRecoveries(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.task_rounds, 7);
}

struct ChangingBadSources(Mutex<usize>);
#[async_trait]
impl LlmClient for ChangingBadSources {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut round = self.0.lock().unwrap();
        *round += 1;
        let mut response = call(
            &format!("bad-{round}"),
            "memory_write",
            json!({"title":"Invalid finding", "summary":"Invalid", "body":"Unsupported claim", "kind":"fact", "source_ids":[format!("invented-{round}")]}),
        );
        response.calls.push(ToolCall {
            id: format!("progress-{round}"),
            name: "task_state".into(),
            arguments: json!({"action":"update","patch":{"findings":[format!("round {round}")]}})
                .to_string(),
        });
        Ok(response)
    }
}
#[tokio::test]
async fn changing_bad_arguments_and_unrelated_writes_cannot_evade_recovery_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.config.stall_round_limit = 3;
    session.add_user("Investigate safely".into());
    let model = Arc::new(ChangingBadSources(Mutex::new(0)));
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(session, model.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(*model.0.lock().unwrap(), 3);
    assert!(
        result
            .last_error
            .unwrap()
            .contains("tool_recovery_limit: memory_write")
    );
    assert_eq!(result.memory.entries.len(), 0);
    assert_eq!(result.task.findings, ["round 2"]); // No later writes after terminal failure.
}
