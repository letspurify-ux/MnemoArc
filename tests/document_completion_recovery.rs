use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall, Usage},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const RESULT: &str = "# Report\nThe requested document is saved and verified.\n";

fn fixture() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("report.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            context_tokens: 128_000,
            run_tokens: 1_000_000,
            output_tokens: 1024,
            stall_round_limit: 3,
            source_answer_review: false,
            source_document_review: false,
            ..Default::default()
        },
    );
    s.add_user("Write report.md with the requested document.".into());
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":"document_edit"}}),
    )
    .unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Report\nDraft.\n"}),
    )
    .unwrap();
    tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Finish the requested document"]}]})).unwrap();
    (dir, s)
}

#[derive(Clone, Copy)]
enum Delay {
    Final,
    InvalidEdit,
    Read,
    Rewrite,
    Empty,
    Bookkeeping,
    InactiveTool,
    UnknownTool,
    Truncated,
}

struct DocumentClient {
    path: PathBuf,
    delay: Delay,
    delay_rounds: usize,
    calls: Mutex<usize>,
    input_usage: usize,
}

fn call(id: usize, name: &str, args: Value) -> Completion {
    Completion {
        calls: vec![ToolCall {
            id: format!("action-{id}"),
            name: name.into(),
            arguments: args.to_string(),
        }],
        ..Default::default()
    }
}

#[async_trait]
impl LlmClient for DocumentClient {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if payload["completion_review"] == true {
            assert!(request.get("tool_choice").is_none());
            let evidence = payload["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["kind"] == "current_file")
                .unwrap();
            let met = evidence["text"] == RESULT;
            return Ok(Completion { text: json!({"checks":payload["criteria"].as_array().unwrap().iter().map(|criterion|
                json!({"id":criterion["id"],"status":if met {"met"} else {"unmet"},"reason":if met {"The current file contains the required document"} else {"Only a draft is saved"}, "evidence":[evidence["id"]],"next_action":if met {""} else {"Finish the requested document"}})
            ).collect::<Vec<_>>()}).to_string(), ..Default::default() });
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        let mut count = self.calls.lock().unwrap();
        *count += 1;
        assert!(
            *count < 100,
            "document recovery failed to finish or honor its budget"
        );
        if matches!(self.delay, Delay::Final | Delay::Empty)
            && *count > 1
            && *count <= self.delay_rounds.saturating_add(1)
        {
            assert_eq!(
                request["tool_choice"], "required",
                "rejected finals must request a concrete action"
            );
        }
        let mut response = if *count <= self.delay_rounds {
            match self.delay {
                Delay::Final => Completion {
                    text: "Done".into(),
                    ..Default::default()
                },
                Delay::Empty => Completion::default(),
                Delay::Truncated => Completion {
                    length_limited: true,
                    discarded_tool_calls: true,
                    ..Default::default()
                },
                Delay::Bookkeeping => {
                    if let Some(id) = state["run_guidance"]["current_todo"]["id"].as_str() {
                        call(
                            *count,
                            "task_plan",
                            json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":id,"result":"Claimed completion without changing the draft"}]}),
                        )
                    } else {
                        Completion {
                            text: "Done".into(),
                            ..Default::default()
                        }
                    }
                }
                Delay::InvalidEdit => call(
                    *count,
                    "document_edit",
                    json!({"action":"append","expected_hash":"wrong-hash","text":"Correction"}),
                ),
                Delay::Read => call(*count, "document_inspect", json!({})),
                Delay::InactiveTool => call(*count, "symbol_search", json!({"query":"run"})),
                Delay::UnknownTool => call(*count, "document_write", json!({"text":"Draft"})),
                Delay::Rewrite => call(
                    *count,
                    "document_edit",
                    json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&self.path)?),"text":format!("# Report\nDraft {}.\n", *count)}),
                ),
            }
        } else if std::fs::read_to_string(&self.path)? != RESULT {
            call(
                *count,
                "document_edit",
                json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&self.path)?),"text":RESULT}),
            )
        } else if let Some(id) = state["run_guidance"]["current_todo"]["id"].as_str() {
            call(
                *count,
                "task_plan",
                json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":id,"result":"Saved the requested document"}]}),
            )
        } else {
            Completion {
                text: "Saved report.md".into(),
                ..Default::default()
            }
        };
        response.usage = Some(Usage {
            input: self.input_usage,
            output: 10,
            cached: None,
        });
        Ok(response)
    }
}

async fn run(s: Session, client: Arc<dyn LlmClient>) -> (Session, Vec<String>) {
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut deltas = Vec::new();
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Delta { text, .. } = event {
                deltas.push(text);
            }
        }
        deltas
    });
    let s = run_session(s, client, CancellationToken::new(), tx).await;
    (s, drain.await.unwrap())
}

#[tokio::test]
async fn document_recovery_can_finish_after_every_old_no_progress_cutoff() {
    for delay in [
        Delay::Final,
        Delay::InvalidEdit,
        Delay::Read,
        Delay::Rewrite,
        Delay::Empty,
        Delay::Bookkeeping,
        Delay::InactiveTool,
        Delay::Truncated,
    ] {
        let (_dir, s) = fixture();
        let path = s.project.output.clone();
        let client = Arc::new(DocumentClient {
            path: path.clone(),
            delay,
            delay_rounds: 40,
            calls: Mutex::new(0),
            input_usage: 1000,
        });
        let (s, deltas) = run(s, client.clone()).await;
        assert_eq!(s.status, "complete", "{:?}", s.last_error);
        assert!(s.completion_review.approved);
        assert!(s.task.current_todo().is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), RESULT);
        assert!(*client.calls.lock().unwrap() > 40);
        assert_eq!(deltas, ["Saved report.md"]);
    }
}

#[tokio::test]
async fn persistent_document_loop_obeys_budget_and_resume_can_finish() {
    let (_dir, mut s) = fixture();
    s.config.run_tokens = 120_000;
    let path = s.project.output.clone();
    let client = Arc::new(DocumentClient {
        path: path.clone(),
        delay: Delay::Final,
        delay_rounds: usize::MAX,
        calls: Mutex::new(0),
        input_usage: 5000,
    });
    let (s, deltas) = run(s, client.clone()).await;
    assert_eq!(s.status, "blocked");
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("run_budget_exhausted")
    );
    assert!(*client.calls.lock().unwrap() > 12);
    assert!(deltas.is_empty());
    assert!(s.task.current_todo().is_some());
    assert!(!s.completion_review.approved);
    let (s, _) = run(
        s,
        Arc::new(DocumentClient {
            path,
            delay: Delay::Read,
            delay_rounds: 0,
            calls: Mutex::new(0),
            input_usage: 1000,
        }),
    )
    .await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(s.completion_review.approved);
}

#[tokio::test]
async fn rejected_acceptance_does_not_stop_a_late_document_repair() {
    let (_dir, mut s) = fixture();
    let path = s.project.output.clone();
    tools::completion_review::begin(&mut s, "Saved report.md").unwrap();
    let request = tools::completion_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let verdict = json!({"checks":payload["criteria"].as_array().unwrap().iter().map(|criterion|
        json!({"id":criterion["id"],"status":"unmet","reason":"Only a draft is saved", "evidence":[],"next_action":"Finish the requested document"})
    ).collect::<Vec<_>>()});
    assert!(
        tools::completion_review::finish(&mut s, &verdict.to_string())
            .unwrap()
            .is_none()
    );
    tools::completion_review::schedule_repairs(&mut s);
    let (s, deltas) = run(
        s,
        Arc::new(DocumentClient {
            path,
            delay: Delay::Read,
            delay_rounds: 40,
            calls: Mutex::new(0),
            input_usage: 1000,
        }),
    )
    .await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(s.completion_review.approved);
    assert_eq!(deltas, ["Saved report.md"]);
}

struct LateAcceptance {
    client: DocumentClient,
    reviews: Mutex<usize>,
    too_many_tools: bool,
}

#[async_trait]
impl LlmClient for LateAcceptance {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if payload["completion_review"] == true {
            let mut reviews = self.reviews.lock().unwrap();
            *reviews += 1;
            if *reviews > 1 {
                assert!(
                    payload["previous_response_error"]
                        .as_str()
                        .unwrap()
                        .starts_with("completion_review_invalid:")
                );
            }
            if *reviews <= 12 {
                return Ok(Completion {
                    text: "invalid JSON".into(),
                    calls: if self.too_many_tools {
                        (0..33)
                            .map(|i| ToolCall {
                                id: format!("forbidden-review-tool-{i}"),
                                name: "document_edit".into(),
                                arguments: json!({"action":"append","text":"must not execute"})
                                    .to_string(),
                            })
                            .collect()
                    } else {
                        vec![]
                    },
                    usage: Some(Usage {
                        input: 1000,
                        output: 10,
                        cached: None,
                    }),
                    ..Default::default()
                });
            }
        }
        self.client.complete(request, config, cancel, tx).await
    }
}

#[tokio::test]
async fn malformed_document_acceptance_can_recover_after_eight_responses() {
    for too_many_tools in [false, true] {
        let (_dir, s) = fixture();
        let client = Arc::new(LateAcceptance {
            client: DocumentClient {
                path: s.project.output.clone(),
                delay: Delay::Read,
                delay_rounds: 0,
                calls: Mutex::new(0),
                input_usage: 1000,
            },
            reviews: Mutex::new(0),
            too_many_tools,
        });
        let (s, deltas) = run(s, client.clone()).await;
        assert_eq!(s.status, "complete", "{:?}", s.last_error);
        assert_eq!(*client.reviews.lock().unwrap(), 13);
        assert!(s.completion_review.approved);
        assert_eq!(deltas, ["Saved report.md"]);
        assert!(
            !s.ledger
                .keys()
                .any(|id| id.starts_with("forbidden-review-tool-"))
        );
    }
}

struct LateCheckpoint {
    client: DocumentClient,
    attempts: Mutex<usize>,
    failures: usize,
}

#[async_trait]
impl LlmClient for LateCheckpoint {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if let Some((_, json)) = content.split_once('\n')
            && let Ok(state) = serde_json::from_str::<Value>(json)
            && state["checkpoint"].is_object()
        {
            let mut attempts = self.attempts.lock().unwrap();
            *attempts += 1;
            assert!(*attempts < 100, "checkpoint failed to honor the run budget");
            if *attempts > 8 {
                assert!(
                    !request["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("LAST cleanup request")
                );
            }
            let id = if *attempts <= self.failures {
                json!("wrong-id")
            } else {
                state["checkpoint"]["id"].clone()
            };
            return Ok(call(
                *attempts + 100,
                "checkpoint_complete",
                json!({
                    "id":id,"progress":"Finish the requested document after cleanup",
                    "no_save_reason":"No additional evidence to save"
                }),
            ));
        }
        self.client.complete(request, config, cancel, tx).await
    }
}

#[tokio::test]
async fn document_checkpoint_can_recover_after_eight_failures() {
    let (_dir, mut s) = fixture();
    mnemoarc::context::ContextManager::prepare(&mut s, 120_000).unwrap();
    assert!(s.checkpoint.is_some());
    let client = Arc::new(LateCheckpoint {
        client: DocumentClient {
            path: s.project.output.clone(),
            delay: Delay::Read,
            delay_rounds: 0,
            calls: Mutex::new(0),
            input_usage: 1000,
        },
        attempts: Mutex::new(0),
        failures: 12,
    });
    let (s, deltas) = run(s, client.clone()).await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert_eq!(*client.attempts.lock().unwrap(), 13);
    assert_eq!(s.checkpoints_completed, 1);
    assert!(s.completion_review.approved);
    assert_eq!(deltas, ["Saved report.md"]);
}

#[tokio::test]
async fn document_checkpoint_stops_at_budget_without_losing_original_context() {
    let (_dir, mut s) = fixture();
    s.config.run_tokens = 200_000;
    mnemoarc::context::ContextManager::prepare(&mut s, 120_000).unwrap();
    let original = s.history.bundles.clone();
    let client = Arc::new(LateCheckpoint {
        client: DocumentClient {
            path: s.project.output.clone(),
            delay: Delay::Read,
            delay_rounds: 0,
            calls: Mutex::new(0),
            input_usage: 1000,
        },
        attempts: Mutex::new(0),
        failures: usize::MAX,
    });
    let (s, deltas) = run(s, client.clone()).await;
    assert_eq!(s.status, "blocked");
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("run_budget_exhausted")
    );
    assert!(*client.attempts.lock().unwrap() > 8);
    assert!(s.checkpoint.is_some());
    assert_eq!(s.checkpoints_completed, 0);
    for bundle in original {
        let retained = s.history.read(bundle.id).unwrap();
        assert!(retained.active);
        assert_eq!(retained.messages, bundle.messages);
    }
    assert!(deltas.is_empty());
}

struct OversizedFinal {
    client: DocumentClient,
    sent: Mutex<bool>,
}

#[async_trait]
impl LlmClient for OversizedFinal {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if !*self.sent.lock().unwrap() {
            *self.sent.lock().unwrap() = true;
            return Ok(Completion {
                text: "Extra final report detail. ".repeat(6000),
                ..Default::default()
            });
        }
        self.client.complete(request, config, cancel, tx).await
    }
}

#[tokio::test]
async fn oversized_final_can_be_shortened_without_stopping_document_work() {
    let (_dir, mut s) = fixture();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write",
        "expected_hash":tools::hash(b"# Report\nDraft.\n"),"text":RESULT}),
    )
    .unwrap();
    let id = s.task.current_todo().unwrap().id.clone();
    let revision = s.task.plan_revision;
    tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":revision,
        "operations":[{"op":"complete","id":id,"result":"Saved the requested document"}]}),
    )
    .unwrap();
    let client = Arc::new(OversizedFinal {
        client: DocumentClient {
            path: s.project.output.clone(),
            delay: Delay::Read,
            delay_rounds: 0,
            calls: Mutex::new(0),
            input_usage: 1000,
        },
        sent: Mutex::new(false),
    });
    let (s, deltas) = run(s, client).await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(s.completion_review.approved);
    assert_eq!(deltas, ["Saved report.md"]);
}

struct OversizedToolBatch {
    client: DocumentClient,
    calls: Mutex<usize>,
    batch_size: usize,
    provider_rejects: bool,
}

struct MalformedProviderBatch {
    client: DocumentClient,
    calls: Mutex<usize>,
    fault: &'static str,
}

#[async_trait]
impl LlmClient for MalformedProviderBatch {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let count = {
            let mut count = self.calls.lock().unwrap();
            *count += 1;
            *count
        };
        if (2..=13).contains(&count) {
            assert_eq!(request["tool_choice"], "required");
            assert_eq!(
                std::fs::read_to_string(&self.client.path)?,
                "# Report\nDraft.\n"
            );
            assert!(request.to_string().contains("none of this batch executed"));
        }
        if count <= 12 {
            if self.fault != "oversized_identity" {
                anyhow::bail!("{}", self.fault);
            }
            let mut response = call(
                800,
                "document_edit",
                json!({"action":"append","expected_hash":tools::hash(&std::fs::read(&self.client.path)?),"text":"Never execute this rejected batch"}),
            );
            let mut invalid = call(801, "document_inspect", json!({}));
            invalid.calls[0].id = "x".repeat(mnemoarc::llm::MAX_TOOL_CALL_ID_BYTES + 1);
            response.calls.extend(invalid.calls);
            return Ok(response);
        }
        self.client.complete(request, config, cancel, tx).await
    }
}

#[tokio::test]
async fn rejected_provider_tool_json_returns_to_document_repair_until_completion() {
    for fault in [
        "invalid_tool_arguments: EOF while parsing an object",
        "malformed_tool_call",
        "oversized_identity",
    ] {
        let (_dir, s) = fixture();
        let client = Arc::new(MalformedProviderBatch {
            client: DocumentClient {
                path: s.project.output.clone(),
                delay: Delay::Read,
                delay_rounds: 0,
                calls: Mutex::new(0),
                input_usage: 1000,
            },
            calls: Mutex::new(0),
            fault,
        });
        let (s, deltas) = run(s, client).await;
        assert_eq!(s.status, "complete", "{fault}: {:?}", s.last_error);
        assert!(s.completion_review.approved);
        assert!(!s.ledger.contains_key("action-800"));
        assert_eq!(deltas, ["Saved report.md"]);
    }
}

#[tokio::test]
async fn unknown_tool_names_can_be_corrected_without_stopping_document_work() {
    let (_dir, s) = fixture();
    let client = Arc::new(DocumentClient {
        path: s.project.output.clone(),
        delay: Delay::UnknownTool,
        delay_rounds: 12,
        calls: Mutex::new(0),
        input_usage: 1000,
    });
    let (s, deltas) = run(s, client).await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(s.completion_review.approved);
    assert_eq!(deltas, ["Saved report.md"]);
}

#[tokio::test]
async fn malformed_provider_recovery_charges_output_and_stops_at_run_budget() {
    let (_dir, mut s) = fixture();
    s.config.run_tokens = 30_000;
    let output_budget = s.config.output_tokens;
    let client = Arc::new(MalformedProviderBatch {
        client: DocumentClient {
            path: s.project.output.clone(),
            delay: Delay::Read,
            delay_rounds: 0,
            calls: Mutex::new(0),
            input_usage: 1000,
        },
        calls: Mutex::new(0),
        fault: "invalid_tool_arguments: incomplete JSON",
    });
    let (s, deltas) = run(s, client.clone()).await;
    let attempts = *client.calls.lock().unwrap();
    assert!((1..12).contains(&attempts));
    assert_eq!(s.status, "blocked");
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("run_budget_exhausted:")
    );
    assert_eq!(s.output_tokens, output_budget * attempts);
    assert!(s.usage_incomplete);
    assert!(!s.completion_review.approved);
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Report\nDraft.\n"
    );
    assert!(deltas.is_empty());
}

#[async_trait]
impl LlmClient for OversizedToolBatch {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if count == 1 {
            if self.provider_rejects {
                anyhow::bail!("tool_call_batch_limit: at most 32 calls");
            }
            let mut batch = call(
                900,
                "document_edit",
                json!({"action":"append",
                "expected_hash":tools::hash(&std::fs::read(&self.client.path)?),"text":"Should never execute"}),
            );
            for i in 1..self.batch_size {
                batch
                    .calls
                    .extend(call(900 + i, "document_inspect", json!({})).calls);
            }
            return Ok(batch);
        }
        if count == 2 {
            assert_eq!(
                std::fs::read_to_string(&self.client.path)?,
                "# Report\nDraft.\n"
            );
            let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1)?;
            assert_eq!(state["run_guidance"]["max_tool_calls"], 2);
            assert!(
                state["run_guidance"]["recovery_reason"]
                    .as_str()
                    .unwrap()
                    .contains("none of this batch executed")
            );
        }
        self.client.complete(request, config, cancel, tx).await
    }
}

#[tokio::test]
async fn oversized_tool_batch_can_be_split_without_executing_rejected_writes() {
    for (batch_size, provider_rejects) in [(3, false), (33, false), (33, true)] {
        let (_dir, mut s) = fixture();
        s.config.batch_tokens = 400;
        s.config.result_tokens = 400;
        let client = Arc::new(OversizedToolBatch {
            client: DocumentClient {
                path: s.project.output.clone(),
                delay: Delay::Read,
                delay_rounds: 0,
                calls: Mutex::new(0),
                input_usage: 1000,
            },
            calls: Mutex::new(0),
            batch_size,
            provider_rejects,
        });
        let (s, deltas) = run(s, client).await;
        assert_eq!(s.status, "complete", "{:?}", s.last_error);
        assert!(!s.ledger.contains_key("action-900"));
        assert!(s.completion_review.approved);
        assert_eq!(deltas, ["Saved report.md"]);
    }
}
