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
                json!({"action":"update","patch":{"current":"compare documents","next":"report"}}),
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
    assert_eq!(result.task.current, "compare documents");
    assert!(result.active_tools.is_empty());
    assert_eq!(result.status, "complete");
}

struct RetryCleanup {
    step: Mutex<usize>,
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
                    "failed-memory",
                    "memory_write",
                    json!({"key":"failed","title":"너무 긴 제목".repeat(150),"summary":"과도한 메타데이터","body":"보존할 근거와 실패 기록".repeat(150),"kind":"fact"}),
                )
            }
            1 => {
                assert!(request.to_string().contains("memory_metadata_limit"));
                Completion {
                    calls: vec![
                        ToolCall{id:format!("ack-{}",state["checkpoint"]["id"]),name:"checkpoint_complete".into(),arguments:json!({"id":state["checkpoint"]["id"],"progress":"필요한 기억을 보존했고 원본 수정 금지를 유지한다. 다음은 문서 작성이다."}).to_string()},
                        ToolCall{id:format!("memory-{}",state["checkpoint"]["id"]),name:"memory_write".into(),arguments:json!({"key":format!("finding-{}",state["recent_memories"].as_array().unwrap().len()),"title":"주요 발견","summary":"실패 원인과 다음 조사","body":"실패한 접근을 반복하지 않고 기존 제약을 지킨다.","kind":"failure"}).to_string()},
                        ToolCall{id:format!("progress-{}",state["checkpoint"]["id"]),name:"task_state".into(),arguments:json!({"action":"update","patch":{"current":"실패 근거와 다음 작업 저장 완료"}}).to_string()},
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
async fn unknown_model_recovers_failed_saves_over_three_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = s(dir.path());
    session.config.model = "z-ai/glm-5.3-flash".into();
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
            } else if id.starts_with("memory-") {
                assert!(result["data"]["acknowledged"].is_null());
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
            !session.history.search("memory_metadata_limit", 0, 50)["items"]
                .as_array()
                .unwrap()
                .is_empty(),
            "failed saves remain retrievable"
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
            .unwrap()
            .contains("checkpoint_retry_limit")
    );
    assert_eq!(result.checkpoints_completed, 0);
    assert!(result.history.bundles.iter().all(|b| b.active));
    assert_eq!(result.checkpoint.unwrap().attempts, 3);
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
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
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
async fn unchanged_parallel_reads_are_suppressed_then_stop_recoverably() {
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
    assert_eq!(result.status, "blocked");
    assert!(result.last_error.unwrap().contains("repeated_work_limit"));
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
    assert_eq!(result.history.bundles.len(), 4);
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
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        *self.calls.lock().unwrap() += 1;
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
    let client = Arc::new(PrematureFinal {
        calls: Mutex::new(0),
    });
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(session, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.status, "partial");
    assert_eq!(*client.calls.lock().unwrap(), 3);
    assert_eq!(result.run_guidance["phase"], "verify");
    assert_eq!(result.history.bundles.len(), 3);
}
