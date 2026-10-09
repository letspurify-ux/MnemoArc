use crate::support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::run_session,
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools::ToolRegistry,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    )
}

#[test]
fn source_document_review_instruction_follows_session_setting() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();

    for enabled in [false, true] {
        s.config.source_document_review = enabled;
        let request = ContextManager::request(&s, vec![]).unwrap();
        let instruction = request["messages"][0]["content"].as_str().unwrap();
        assert_eq!(
            instruction
                .contains("The separate source-document review is enabled for this session."),
            enabled
        );
        assert_eq!(
            instruction
                .contains("The separate source-document review is disabled for this session."),
            !enabled
        );
        assert!(
            !instruction
                .contains("Source-document work also requires the separate document review.")
        );
    }
}

async fn run(s: Session, client: Arc<dyn LlmClient>) -> Session {
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

struct CapacityProbe(std::sync::atomic::AtomicUsize);

#[async_trait]
impl LlmClient for CapacityProbe {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Completion {
            text: "A final answer must not bypass retained-state limits.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn resuming_over_capacity_cannot_keep_appending_final_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.memory_bytes = 32 * 1024;
    s.add_user("Keep the existing write receipt".into());
    s.ledger.insert(
        "retained-write".into(),
        (
            "file_write:receipt".into(),
            json!({"status":"ok","receipt":"x".repeat(s.config.memory_bytes)}),
        ),
    );
    let receipt = s.ledger.clone();
    let history = s.history.bytes();
    let client = Arc::new(CapacityProbe(std::sync::atomic::AtomicUsize::new(0)));
    for _ in 0..3 {
        s = run(s, client.clone()).await;
        assert_eq!(s.status, "blocked");
        assert!(
            s.last_error
                .as_deref()
                .unwrap()
                .starts_with("session_metadata_capacity")
        );
        assert_eq!(
            s.ledger, receipt,
            "write receipts must survive capacity failures"
        );
        assert_eq!(s.history.bytes(), history);
    }
    assert_eq!(client.0.load(std::sync::atomic::Ordering::Relaxed), 0);
}

struct ReceiptCapacityProbe;

#[async_trait]
impl LlmClient for ReceiptCapacityProbe {
    async fn complete(
        &self,
        _: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        Ok(Completion {
            calls: ["first.txt", "second.txt", "third.txt"]
                .into_iter()
                .map(|path| ToolCall {
                    id: path.into(),
                    name: "file_write".into(),
                    arguments: json!({"path":path,"content":"x".repeat(config.memory_bytes)})
                        .to_string(),
                })
                .collect(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_tool_batch_stops_starting_writes_after_retained_receipts_exceed_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.memory_bytes = 32 * 1024;
    s.add_user("Save the files while preserving every successful write receipt".into());
    let capacity = s.config.memory_bytes;
    let result = run(s, Arc::new(ReceiptCapacityProbe)).await;
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("session_metadata_capacity")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("first.txt")).unwrap(),
        "x".repeat(capacity)
    );
    assert!(!dir.path().join("second.txt").exists());
    assert!(!dir.path().join("third.txt").exists());
    assert!(result.ledger.contains_key("first.txt"));
    assert!(!result.ledger.contains_key("second.txt"));
    assert!(!result.ledger.contains_key("third.txt"));
}

const ORIGINAL: &str = "Explain the source module and preserve the original requirements";
const MAINTENANCE: &str = "Clean up memory and progress. Do not modify project files.";

struct Maintenance {
    cancel: bool,
}
#[async_trait]
impl LlmClient for Maintenance {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let messages = request["messages"].as_array().unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message["content"] == MAINTENANCE)
        );
        assert!(
            messages
                .iter()
                .all(|message| message.get("maintenance").is_none())
        );
        if self.cancel {
            cancel.cancel();
            anyhow::bail!("cancelled");
        }
        Ok(Completion {
            text: "Memory is organized; the original task is preserved.".into(),
            ..Default::default()
        })
    }
}

struct OriginalTask;
#[async_trait]
impl LlmClient for OriginalTask {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        assert!(!request.to_string().contains(MAINTENANCE));
        let state = request["messages"][1]["content"].as_str().unwrap();
        if let Some(snapshot) = state.strip_prefix("Saved task snapshot:\n") {
            let snapshot: Value = serde_json::from_str(snapshot)?;
            assert_eq!(snapshot["original_request"], ORIGINAL);
        } else {
            assert!(request.to_string().contains(ORIGINAL));
        }
        Ok(Completion {
            text: "The original source explanation remains the task.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn cleanup_preserves_request_and_receipts_and_expires_its_instruction_on_success_or_cancel() {
    for cancelled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        s.add_user(ORIGINAL.into());
        s.ledger.insert(
            "saved-call".into(),
            ("file_write:receipt".into(), json!({"status":"ok"})),
        );
        let ledger = s.ledger.clone();
        let criteria = s.task.completion.clone();
        s.add_maintenance(MAINTENANCE.into());
        assert_eq!(s.latest_request, ORIGINAL);
        assert_eq!(s.ledger, ledger);
        let mut result = run(s, Arc::new(Maintenance { cancel: cancelled })).await;
        assert_eq!(
            result.status,
            if cancelled { "cancelled" } else { "complete" },
            "{:?}",
            result.last_error
        );
        assert_eq!(result.latest_request, ORIGINAL);
        assert_eq!(result.task.completion, criteria);
        assert_eq!(result.ledger, ledger);
        let maintenance = result
            .history
            .bundles
            .iter()
            .find(|bundle| {
                bundle
                    .messages
                    .iter()
                    .any(|message| message["maintenance"] == true)
            })
            .unwrap();
        assert!(!maintenance.active);
        assert!(maintenance.reviewed);
        let request = ContextManager::request(&result, ToolRegistry::definitions(&result)).unwrap();
        assert!(!request.to_string().contains(MAINTENANCE));
        result
            .queue_question("What was the original task?".into())
            .unwrap();
        result = run(result, Arc::new(OriginalTask)).await;
        assert_eq!(result.run_history.back().unwrap().reason, "complete");
        result = run(result, Arc::new(OriginalTask)).await;
        assert_eq!(result.status, "complete", "{:?}", result.last_error);
        assert_eq!(result.latest_request, ORIGINAL);
    }
}

struct Pagination {
    step: Mutex<usize>,
}

#[tokio::test]
async fn cancelled_cleanup_retains_history_referenced_by_a_pending_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user(ORIGINAL.into());
    // The original conversation was checkpointed before cleanup began.
    for bundle in &mut s.history.bundles {
        bundle.active = false;
        bundle.reviewed = true;
    }
    s.add_maintenance(MAINTENANCE.into());
    let budget = ContextManager::input_budget(&s.config);
    ContextManager::prepare(&mut s, budget).unwrap();
    let id = s.history.bundles.back().unwrap().id;
    assert!(s.checkpoint.as_ref().unwrap().bundle_ids.contains(&id));
    let mut result = run(s, Arc::new(Maintenance { cancel: true })).await;
    assert_eq!(result.status, "cancelled");
    let bundle = result.history.read(id).unwrap();
    assert!(!bundle.active);
    assert!(!bundle.reviewed);
    assert!(result.history.prune(1).is_err());
    result.checkpoint.as_mut().unwrap().acknowledged = true;
    ContextManager::commit(&mut result).unwrap();
    assert!(result.history.read(id).unwrap().reviewed);
    result.history.prune(1).unwrap();
}
#[async_trait]
impl LlmClient for Pagination {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        let results: Vec<Value> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| serde_json::from_str(message["content"].as_str().unwrap()).unwrap())
            .collect();
        let calls = match *step {
            0 => ["src", "tests"]
                .into_iter()
                .map(|scope| ToolCall {
                    id: format!("first-{scope}"),
                    name: "file_list".into(),
                    arguments: json!({"mode":"paths","path_glob":format!("{scope}/**"),"limit":1})
                        .to_string(),
                })
                .collect(),
            1 => {
                assert_eq!(results.len(), 2);
                results
                    .iter()
                    .zip(["src", "tests"])
                    .map(|(result, scope)| {
                        assert_eq!(result["status"], "ok", "{result}");
                        assert_eq!(result["data"]["paths"], json!([format!("{scope}/a.rs")]));
                        let cursor = result["data"]["next_cursor"].as_str().unwrap();
                        ToolCall {
                            id: format!("next-{scope}"),
                            name: "file_list".into(),
                            arguments: json!({"cursor":cursor,"limit":1}).to_string(),
                        }
                    })
                    .collect()
            }
            2 => {
                assert_eq!(results.len(), 4);
                for (result, scope) in results[2..].iter().zip(["src", "tests"]) {
                    assert_eq!(result["status"], "ok", "{result}");
                    assert_eq!(result["data"]["paths"], json!([format!("{scope}/b.rs")]));
                    assert!(result["data"]["next_cursor"].is_null());
                }
                vec![]
            }
            _ => panic!("pagination must finish without restarting expired cursors"),
        };
        *step += 1;
        Ok(Completion {
            text: if calls.is_empty() {
                "Listed all scoped files.".into()
            } else {
                String::new()
            },
            calls,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn independent_file_list_cursors_survive_parallel_workers_and_continue_with_cursor_only() {
    let dir = tempfile::tempdir().unwrap();
    for scope in ["src", "tests"] {
        std::fs::create_dir(dir.path().join(scope)).unwrap();
        for file in ["a.rs", "b.rs"] {
            std::fs::write(dir.path().join(scope).join(file), "fn example() {}\n").unwrap();
        }
    }
    std::fs::write(dir.path().join("outside.rs"), "fn outside() {}\n").unwrap();
    let mut s = session(dir.path());
    s.config.read_parallelism = 2;
    s.add_user("List the source files and tests separately".into());
    let client = Arc::new(Pagination {
        step: Mutex::new(0),
    });
    let result = run(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(*client.step.lock().unwrap(), 3);
    assert_eq!(result.list_cursor_scopes.len(), 2);
}
