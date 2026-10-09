use crate::support;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient, ToolCall, Usage},
    session::{Closing, Session},
    tools,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fixture(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            output: root.join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.add_user("Write the original document".into());
    s.select_workflow("source_document").unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Original\nSaved draft.\n"}),
    )
    .unwrap();
    tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Fix the existing diagram"]}]})).unwrap();
    s.document_review.issues = vec!["The approved branch loops backwards".into()];
    s.document_review.pending = true;
    s.completion_gaps = vec!["Diagram still needs repair".into()];
    s.progress_recovery.closing = Some(Closing {
        reason: "budget".into(),
        ..Default::default()
    });
    s.run_guidance = json!({"phase":"verify","remaining_tokens":123});
    s.task_rounds = 100;
    s.continuation = Some(false);
    s.status = "blocked".into();
    s.last_error = Some("run_budget_exhausted: insufficient budget".into());
    let budget = ContextManager::input_budget(&s.config);
    ContextManager::prepare(&mut s, budget).unwrap();
    assert!(s.checkpoint.is_some());
    s.checkpoint.as_mut().unwrap().attempts = 3;
    s.checkpoint.as_mut().unwrap().failed_attempts = 2;
    s
}

fn preserved(s: &Session) -> Value {
    json!({"task":s.task,"document_review":s.document_review,
        "checkpoint":s.checkpoint,"gaps":s.completion_gaps,"request":s.latest_request,
        "status":s.status,"error":s.last_error,"activity":s.activity,"rounds":s.task_rounds,
        "continuation":s.continuation,"workflow":s.workflow_mode,"written":s.document_written,
        "last_write":s.last_document_write,"guidance":s.run_guidance,
        "active_history":s.history.active(),"ledger":s.ledger,"sources":s.sources,
        "memory_generation":s.memory.generation,"recovery":format!("{:?}",s.progress_recovery)})
}

enum Reply {
    Answer,
    Tool,
    Fail,
    Cancel,
    Wait,
    Panic,
    Length,
}

#[test]
fn invalid_questions_do_not_change_the_task_or_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    let before = preserved(&s);
    let history = serde_json::to_value(&s.history.bundles).unwrap();
    assert!(s.queue_question("   ".into()).is_err());
    s.config.history_bytes = s.history.bytes() + 1;
    assert!(
        s.queue_question("Why did it stop?".into())
            .unwrap_err()
            .to_string()
            .starts_with("history_capacity")
    );
    assert_eq!(preserved(&s), before);
    assert_eq!(serde_json::to_value(&s.history.bundles).unwrap(), history);
    assert!(s.question.is_none());
}
#[async_trait]
impl LlmClient for Reply {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        assert!(request.get("tools").is_none());
        let state: Value = serde_json::from_str(
            request["messages"][1]["content"]
                .as_str()
                .unwrap()
                .strip_prefix("Saved task snapshot:\n")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(state["original_request"], "Write the original document");
        assert_eq!(state["task_status"], "blocked");
        assert_eq!(
            state["document_review"]["issues"][0],
            "The approved branch loops backwards"
        );
        let mut response = Completion {
            text: "The budget was exhausted; the diagram still needs repair.".into(),
            usage: Some(Usage {
                input: 120,
                output: 20,
                cached: Some(10),
            }),
            ..Default::default()
        };
        match self {
            Self::Answer => {}
            Self::Tool => response.calls.push(ToolCall {
                id: "bad-write".into(),
                name: "document_edit".into(),
                arguments: json!({"action":"write","text":"# Replaced\nWrong."}).to_string(),
            }),
            Self::Fail => anyhow::bail!("provider_error: fixture failure"),
            Self::Cancel => {
                cancel.cancel();
                anyhow::bail!("cancelled");
            }
            Self::Wait => return std::future::pending().await,
            Self::Panic => panic!("fixture question panic"),
            Self::Length => response.length_limited = true,
        }
        Ok(response)
    }
}

async fn run(s: Session, client: Arc<dyn LlmClient>) -> Session {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

struct UpdatedConfig;
#[async_trait]
impl LlmClient for UpdatedConfig {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        assert_eq!(config.model, "gpt-4.1");
        assert_eq!(request["model"], "gpt-4.1");
        assert_eq!(config.request_timeout_secs, 37);
        assert_eq!(
            config.api_key.as_ref().map(|key| key.0.as_str()),
            Some("updated-follow-up-key")
        );
        Reply::Answer.complete(request, config, cancel, delta).await
    }
}

#[tokio::test]
async fn follow_up_applies_pending_and_queued_settings_without_changing_the_suspended_task() {
    use mnemoarc::agent::{RunCommand, run_session_controlled};
    for queued in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fixture(dir.path());
        let before = preserved(&s);
        let mut next = s.config.clone();
        next.model = "gpt-4.1".into();
        next.request_timeout_secs = 37;
        next.api_key = Some(mnemoarc::config::Secret("updated-follow-up-key".into()));
        s.queue_question("Why did it stop?".into()).unwrap();
        let (commands, rx_commands) = mpsc::channel(1);
        if queued {
            commands
                .send(RunCommand::Configure(Box::new(next)))
                .await
                .unwrap();
        } else {
            s.pending_config = Some(next);
        }
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session_controlled(
            s,
            Arc::new(UpdatedConfig),
            CancellationToken::new(),
            tx,
            rx_commands,
        )
        .await;
        drain.await.unwrap();
        assert_eq!(preserved(&result), before);
        assert_eq!(result.config.model, "gpt-4.1");
        assert!(result.pending_config.is_none());
        assert_eq!(result.run_history.back().unwrap().reason, "complete");
    }
}

struct NoQuestionRequest;
#[async_trait]
impl LlmClient for NoQuestionRequest {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        panic!("incompatible pending settings must not use the previous model");
    }
}

#[tokio::test]
async fn follow_up_rejects_incompatible_settings_without_pruning_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    let before = preserved(&s);
    s.queue_question("Why did it stop?".into()).unwrap();
    let history = serde_json::to_value(&s.history.bundles).unwrap();
    let mut next = s.config.clone();
    next.history_bytes = 1;
    s.pending_config = Some(next);
    let result = run(s, Arc::new(NoQuestionRequest)).await;
    assert_eq!(preserved(&result), before);
    assert_eq!(
        serde_json::to_value(&result.history.bundles).unwrap(),
        history
    );
    assert!(result.pending_config.is_some());
    assert_eq!(result.config.model, "gpt-4o");
    assert_eq!(
        result.run_history.back().unwrap().reason,
        "settings_pending_cleanup"
    );
    assert_eq!(result.run_history.back().unwrap().rounds, 0);
}

#[tokio::test]
async fn questions_preserve_unfinished_work_even_on_failure_or_cancellation() {
    for (reply, expected) in [
        (Reply::Answer, "complete"),
        (Reply::Tool, "question_tools_not_allowed"),
        (Reply::Fail, "provider_error"),
        (Reply::Cancel, "cancelled"),
        (Reply::Wait, "run_timeout"),
        (Reply::Panic, "model_worker_panic"),
        (Reply::Length, "question_answer_truncated"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fixture(dir.path());
        s.config.run_timeout_secs = 1;
        let before = preserved(&s);
        s.queue_question("Why did it stop?".into()).unwrap();
        assert_eq!(preserved(&s), before);
        let result = run(s, Arc::new(reply)).await;
        assert_eq!(preserved(&result), before, "{expected}");
        assert_eq!(
            std::fs::read_to_string(&result.project.output).unwrap(),
            "# Original\nSaved draft.\n"
        );
        assert!(result.question.is_none());
        let record = result.run_history.back().unwrap();
        assert_eq!(record.reason, expected);
        assert_eq!(record.workflow, "follow_up");
        assert_eq!(record.request, "Why did it stop?");
        assert_eq!(record.last_stage, "question");
        assert!(!record.checkpoint_pending); // The original task's checkpoint was not executed.
        assert_eq!(record.rounds, 1);
        if expected == "complete" {
            assert_eq!((record.input_tokens, record.output_tokens), (120, 20));
            assert_eq!(result.history.bundles.back().unwrap().messages.len(), 2);
        }
    }
}

/// The first follow-up request times out at the provider; the second is
/// answered or times out too.
struct Unanswered {
    answer_second: bool,
    requests: std::sync::Mutex<Vec<(Value, bool)>>,
}
#[async_trait]
impl LlmClient for Unanswered {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let mut requests = self.requests.lock().unwrap();
        requests.push((request, config.retry_timeouts));
        if requests.len() == 1 || !self.answer_second {
            anyhow::bail!(
                "provider_stream_error: {{\"code\":504,\"message\":\"Upstream idle timeout exceeded\"}}"
            );
        }
        Ok(Completion {
            text: "The budget was exhausted.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn an_unanswered_question_is_asked_once_more_with_half_the_snapshot() {
    for answer_second in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fixture(dir.path());
        s.completion_gaps.push("긴 미확인 항목. ".repeat(150));
        let before = preserved(&s);
        s.queue_question("Why did it stop?".into()).unwrap();
        let client = Arc::new(Unanswered {
            answer_second,
            requests: Default::default(),
        });
        let result = run(s, client.clone()).await;
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        // The client does not wait out another timeout of the same request.
        assert!(requests.iter().all(|(_, retry)| !retry));
        let snapshot = |request: &Value| request["messages"][1]["content"].as_str().unwrap().len();
        assert!(snapshot(&requests[1].0) < snapshot(&requests[0].0));
        assert_eq!(preserved(&result), before);
        let record = result.run_history.back().unwrap();
        let expected = if answer_second {
            "complete"
        } else {
            "question_unanswered"
        };
        assert_eq!(record.reason, expected);
    }
}

struct Resume;
#[async_trait]
impl LlmClient for Resume {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let text = request.to_string();
        assert!(text.contains("Write the original document"));
        assert!(!text.contains("Why did it stop?"));
        assert!(request.get("tools").is_some());
        anyhow::bail!("resume_checked: original task received")
    }
}

#[tokio::test]
async fn a_question_does_not_replace_the_request_when_resuming() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    s.queue_question("Why did it stop?".into()).unwrap();
    let s = run(s, Arc::new(Reply::Answer)).await;
    let mut s = run(s, Arc::new(Resume)).await;
    assert_eq!(s.run_history.back().unwrap().reason, "resume_checked");
    assert_eq!(s.latest_request, "Write the original document");
    s.start_new_task("A different task".into());
    assert!(s.document_review.issues.is_empty());
    assert!(s.task.todos.is_empty());
    assert!(s.checkpoint.is_none());
    assert_eq!(s.run_history.len(), 2);
}
