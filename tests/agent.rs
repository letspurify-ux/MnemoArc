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
                .send(RunCommand::Configure(self.config.clone()))
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
