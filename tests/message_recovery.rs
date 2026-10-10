use crate::support;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, RunCommand, run_session, run_session_controlled},
    config::{Config, Project},
    context,
    llm::{Completion, LlmClient, ToolCall, Usage},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const ORIGINAL: &str = "sample.rs를 조사해서 전체 문서를 작성해줘. 원본 파일은 변경하지 마.";
const CHANGE: &str = "요청 범위를 첫 장으로 바꿔서 문서를 작성해줘.";
const GOAL: &str = "sample.rs를 조사해서 첫 장만 작성한다. 원본 파일은 변경하지 않는다.";

enum Reply {
    Text(Completion),
    Cancel,
    WorkReached,
    /// The provider's idle timeout cut the request off.
    Timeout,
}
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Value>>,
    retry_timeouts: Mutex<Vec<bool>>,
}
#[async_trait]
impl LlmClient for Script {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        self.requests.lock().unwrap().push(request);
        self.retry_timeouts
            .lock()
            .unwrap()
            .push(config.retry_timeouts);
        match self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request")
        {
            Reply::Text(completion) => Ok(completion),
            Reply::Cancel => {
                cancel.cancel();
                anyhow::bail!("cancelled");
            }
            Reply::WorkReached => {
                anyhow::bail!("work_reached: document execution received the amended task")
            }
            Reply::Timeout => anyhow::bail!(
                "provider_stream_error: {{\"code\":504,\"message\":\"Upstream idle timeout exceeded\"}}"
            ),
        }
    }
}
fn text(value: impl Into<String>) -> Reply {
    Reply::Text(Completion {
        text: value.into(),
        ..Default::default()
    })
}
fn work(goal: &str) -> Reply {
    text(json!({"intent":"work","authorization_quote":"문서를 작성해줘","changes":{"goal":goal,"completion":["첫 장만 작성"]}}).to_string())
}
async fn execute(s: Session, replies: Vec<Reply>) -> (Session, Vec<Value>) {
    execute_traced(s, replies).await.0
}
/// Also returns whether each request let the client retry timeouts.
async fn execute_traced(s: Session, replies: Vec<Reply>) -> ((Session, Vec<Value>), Vec<bool>) {
    let client = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        requests: Mutex::new(vec![]),
        retry_timeouts: Mutex::new(vec![]),
    });
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert!(
        client.replies.lock().unwrap().is_empty(),
        "unconsumed scripted responses: {:?}",
        result.last_error
    );
    let requests = client.requests.lock().unwrap().clone();
    let retry_timeouts = client.retry_timeouts.lock().unwrap().clone();
    ((result, requests), retry_timeouts)
}
async fn cancelled(root: &std::path::Path) -> Session {
    std::fs::write(root.join("sample.rs"), "pub fn count() -> usize { 7 }\n").unwrap();
    let mut s = Session::new(
        Project {
            root: root.into(),
            output: root.join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            context_tokens: 64000,
            output_tokens: 1024,
            ..support::compact_config()
        },
    );
    s.select_workflow("source_document").unwrap();
    s.receive_message(ORIGINAL.into()).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Draft\nSaved draft.\n"}),
    )
    .unwrap();
    tools::execute(&mut s,"task_plan",json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Write the requested chapter"]}]})).unwrap();
    let (s, _) = execute(s, vec![Reply::Cancel]).await;
    assert_eq!(s.status, "cancelled");
    assert_eq!(s.run_history.back().unwrap().reason, "cancelled");
    s
}
fn preserved(s: &Session) -> Value {
    json!({"goal":s.latest_request,"original":s.original_request,"task":s.task,
        "amendments":s.task_amendments,
        "last_write":s.last_document_write,"checkpoint":s.checkpoint})
}

#[tokio::test]
async fn a_cancelled_document_accepts_the_changed_prompt_after_invalid_classification() {
    let invalid = [
        "네, 첫 장을 작성하겠습니다.".into(),
        json!({"classification":"work","authorization_quote":"문서를 작성해줘","changes":{"goal":GOAL}}).to_string(),
        json!({"intent":"work","authorization_quote":"첫 장만 작성해줘","changes":{"goal":GOAL}}).to_string(),
        json!({"intent":"work","authorization_quote":"문서를 작성해줘","changes":{"completion":["첫 장만 작성"]}}).to_string(),
        json!({"intent":"resume","authorization_quote":"문서를 작성해줘","changes":null}).to_string(),
        json!({"intent":"work","authorization_quote":"문서를 작성해줘","changes":{"goal":"","completion":["첫 장만 작성"]}}).to_string(),
    ];
    for reply in invalid {
        let dir = tempfile::tempdir().unwrap();
        let mut s = cancelled(dir.path()).await;
        let plan = s.task.todos.clone();
        let saved = std::fs::read(&s.project.output).unwrap();
        s.receive_message(CHANGE.into()).unwrap();
        let (s, requests) = execute(s, vec![text(reply), work(GOAL), Reply::WorkReached]).await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0]["response_format"]["json_schema"]["strict"],
            true
        );
        // The schema survives a provider's response_format compatibility
        // fallback because it is also carried in a system message.
        let schema = requests[0]["response_format"]["json_schema"]["schema"].to_string();
        assert!(
            requests[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(&schema)
        );
        let corrected: Value =
            serde_json::from_str(requests[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert!(
            corrected["program_feedback"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        assert_eq!(corrected["current_message"], CHANGE);
        assert_eq!(s.latest_request, GOAL);
        assert_eq!(s.original_request, ORIGINAL);
        assert_eq!(s.task_amendments.len(), 1);
        assert_eq!(s.task.todos.len(), plan.len());
        assert_eq!(s.task.todos[0].id, plan[0].id);
        assert_eq!(s.run_history.back().unwrap().workflow, "source_document");
        assert!(s.last_error.as_deref().unwrap().starts_with("work_reached"));
        assert_eq!(std::fs::read(&s.project.output).unwrap(), saved);
    }
}

#[tokio::test]
async fn truncated_or_tool_bearing_classifications_are_corrected_without_executing_tools() {
    for completion in [
        Completion {
            text: "{\"intent\":\"work\"".into(),
            length_limited: true,
            ..Default::default()
        },
        Completion {
            discarded_tool_calls: true,
            ..Default::default()
        },
        Completion {
            calls: vec![ToolCall {
                id: "forbidden-write".into(),
                name: "document_edit".into(),
                arguments: json!({"action":"create","text":"Forbidden"}).to_string(),
            }],
            ..Default::default()
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = cancelled(dir.path()).await;
        let saved = std::fs::read(&s.project.output).unwrap();
        s.receive_message(CHANGE.into()).unwrap();
        let (s, requests) = execute(
            s,
            vec![Reply::Text(completion), work(GOAL), Reply::WorkReached],
        )
        .await;
        assert_eq!(requests.len(), 3);
        assert_eq!(s.latest_request, GOAL);
        assert_eq!(std::fs::read(&s.project.output).unwrap(), saved);
    }
}

#[tokio::test]
async fn an_unanswered_classification_is_asked_once_more_with_half_the_context() {
    // The request is not retried as is by the client, the next one carries
    // half the optional context, and a second request without an answer
    // gives up.
    for answered in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = cancelled(dir.path()).await;
        s.history.push(
            vec![json!({"role":"assistant","content":"긴 이전 답변. ".repeat(400)})],
            true,
        );
        let before = preserved(&s);
        s.receive_message(CHANGE.into()).unwrap();
        let second = if answered {
            work(GOAL)
        } else {
            // The output ran out on reasoning before any JSON.
            Reply::Text(Completion {
                length_limited: true,
                ..Default::default()
            })
        };
        let mut replies = vec![Reply::Timeout, second];
        if answered {
            replies.push(Reply::WorkReached);
        }
        let ((s, requests), retry_timeouts) = execute_traced(s, replies).await;
        assert_eq!(retry_timeouts[..2], [false, false]);
        let context = |request: &Value| {
            let payload: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            payload["optional_context"].to_string().len()
        };
        assert!(context(&requests[1]) < context(&requests[0]));
        if answered {
            assert_eq!(s.latest_request, GOAL);
        } else {
            assert_eq!(requests.len(), 2);
            assert_eq!(preserved(&s), before);
            assert_eq!(
                s.run_history.back().unwrap().reason,
                "message_routing_unanswered"
            );
        }
    }
}

#[tokio::test]
async fn repeated_invalid_classification_preserves_work_and_a_later_submission_can_recover() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    let before = preserved(&s);
    s.receive_message(CHANGE.into()).unwrap();
    let (mut s, requests) = execute(s, (0..3).map(|_| text("bad JSON")).collect()).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(preserved(&s), before);
    assert_eq!(s.status, "cancelled");
    assert!(s.question.is_none());
    let failed = s.run_history.back().unwrap().clone();
    assert_eq!(failed.workflow, "message_routing");
    assert_eq!(failed.last_stage, "message_routing");
    assert_eq!(failed.reason, "message_routing_invalid");
    s.receive_message(CHANGE.into()).unwrap();
    let (s, _) = execute(s, vec![work(GOAL), Reply::WorkReached]).await;
    assert_eq!(s.latest_request, GOAL);
    assert_eq!(s.task_amendments.len(), 1);
    assert_eq!(s.run_history[s.run_history.len() - 2].id, failed.id);
}

#[tokio::test]
async fn cancellation_during_classification_repair_never_applies_the_new_goal() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    let before = preserved(&s);
    s.receive_message(CHANGE.into()).unwrap();
    let (s, requests) = execute(s, vec![text("bad JSON"), Reply::Cancel]).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(preserved(&s), before);
    assert_eq!(s.run_history.back().unwrap().reason, "cancelled");
    assert_eq!(s.run_history.back().unwrap().workflow, "message_routing");
}

#[tokio::test]
async fn classification_repair_obeys_the_run_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    s.config.run_tokens = 20000;
    let before = preserved(&s);
    s.receive_message(CHANGE.into()).unwrap();
    let (s, requests) = execute(
        s,
        vec![Reply::Text(Completion {
            text: "bad JSON".into(),
            usage: Some(Usage {
                input: 20000,
                output: 0,
                cached: None,
            }),
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(requests.len(), 1);
    assert_eq!(preserved(&s), before);
    assert_eq!(s.run_history.back().unwrap().reason, "run_budget_exhausted");
}

#[tokio::test]
async fn resume_and_repeated_effective_prompt_do_not_depend_on_classification() {
    for message in ["계속 진행", ORIGINAL] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = cancelled(dir.path()).await;
        s.receive_message(message.into()).unwrap();
        let (s, requests) = execute(s, vec![Reply::WorkReached]).await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].get("response_format").is_none());
        assert_eq!(s.latest_request, ORIGINAL);
        assert_eq!(s.run_history.back().unwrap().workflow, "source_document");
    }
}

#[tokio::test]
async fn a_large_old_conversation_cannot_overflow_the_classification_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    s.config.context_tokens = 16000;
    s.config.output_tokens = 1000;
    s.config.batch_tokens = 2000;
    s.config.result_tokens = 1000;
    s.config.checkpoint_tokens = 500;
    s.config.validate().unwrap();
    for _ in 0..8 {
        s.history.push(vec![json!({"role":"assistant","content":"수집한 소스 근거로 문서의 각 항목을 설명합니다. ".repeat(500)})],true);
        // Checkpointed drafts remain in retained history but no longer belong
        // to the main model context. Classification used to pull them back in.
        let archived = s.history.bundles.back_mut().unwrap();
        archived.active = false;
        archived.reviewed = true;
    }
    s.receive_message(CHANGE.into()).unwrap();
    let config = s.config.clone();
    let (s, requests) = execute(s, vec![work(GOAL), Reply::WorkReached]).await;
    assert_eq!(s.latest_request, GOAL);
    assert_eq!(s.task_amendments.len(), 1);
    assert!(
        context::count(&requests[0], &config.model) + config.output_tokens + 1024
            <= config.context_tokens
    );
}

#[tokio::test]
async fn the_full_current_message_and_user_requirements_survive_context_fitting() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    let prompt = format!("{} {}", "추가 조건을 모두 유지한다. ".repeat(700), CHANGE);
    assert!(prompt.chars().count() > 8000);
    let criteria = s.user_criteria.clone();
    s.receive_message(prompt.clone()).unwrap();
    let (s, requests) = execute(s, vec![work(&prompt), Reply::WorkReached]).await;
    let payload: Value =
        serde_json::from_str(requests[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["current_message"], prompt);
    assert_eq!(payload["current_goal"], ORIGINAL);
    assert_eq!(payload["user_criteria"], json!(criteria));
    assert_eq!(s.latest_request, prompt);
}

#[tokio::test]
async fn genuinely_oversized_requirements_fail_without_truncating_or_changing_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    let before = preserved(&s);
    s.receive_message(format!(
        "{} {CHANGE}",
        "별도 조건을 유지한다. ".repeat(12000)
    ))
    .unwrap();
    let (s, requests) = execute(s, vec![]).await;
    assert!(requests.is_empty());
    assert_eq!(preserved(&s), before);
    assert_eq!(
        s.run_history.back().unwrap().reason,
        "message_routing_context_limit"
    );
}

struct ChangeSettings {
    commands: mpsc::Sender<RunCommand>,
    round: Mutex<usize>,
    during_routing: bool,
}
#[async_trait]
impl LlmClient for ChangeSettings {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let round = {
            let mut next = self.round.lock().unwrap();
            let round = *next;
            *next += 1;
            round
        };
        let changing = if self.during_routing { 0 } else { 1 };
        if round > changing {
            assert_eq!(
                config.model, "gpt-4.1",
                "next request must apply the queued setting"
            );
            assert_eq!(request["model"], "gpt-4.1");
            assert_eq!(config.request_timeout_secs, 37);
            assert_eq!(
                config.api_key.as_ref().map(|key| key.0.as_str()),
                Some("changed-key")
            );
            if let Some(definitions) = request["tools"].as_array() {
                assert!(
                    definitions
                        .iter()
                        .any(|tool| tool["function"]["name"] == "symbol_search")
                );
                assert!(
                    !definitions
                        .iter()
                        .any(|tool| tool["function"]["name"] == "symbol_read")
                );
            }
        }
        if round == changing {
            let mut next = config.clone();
            next.model = "gpt-4.1".into();
            next.request_timeout_secs = 37;
            next.api_key = Some(mnemoarc::config::Secret("changed-key".into()));
            self.commands
                .send(RunCommand::Configure(Box::new(next)))
                .await
                .unwrap();
            self.commands
                .send(RunCommand::Tools(["symbol_search".into()].into()))
                .await
                .unwrap();
        }
        let response = if self.during_routing && round == 0 {
            Completion {
                text: "bad routing JSON".into(),
                ..Default::default()
            }
        } else if round == if self.during_routing { 1 } else { 0 } {
            Completion {
                text: json!({"intent":"discuss","authorization_quote":"","changes":null})
                    .to_string(),
                ..Default::default()
            }
        } else if !self.during_routing && round == 1 {
            Completion {
                calls: vec![ToolCall {
                    id: "read-source".into(),
                    name: "file_read".into(),
                    arguments: json!({"path":"sample.rs","start_line":1,"max_lines":1}).to_string(),
                }],
                ..Default::default()
            }
        } else {
            Completion {
                text: "수집한 근거로 답변합니다.".into(),
                ..Default::default()
            }
        };
        Ok(response)
    }
}

#[tokio::test]
async fn settings_and_tools_changed_during_routing_or_collection_apply_to_the_next_request() {
    for during_routing in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = cancelled(dir.path()).await;
        let before = preserved(&s);
        s.receive_message("sample.rs 내용을 읽고 설명해줘.".into())
            .unwrap();
        let (commands, command_rx) = mpsc::channel(4);
        let client = Arc::new(ChangeSettings {
            commands,
            round: Mutex::new(0),
            during_routing,
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result =
            run_session_controlled(s, client.clone(), CancellationToken::new(), tx, command_rx)
                .await;
        drain.await.unwrap();
        assert_eq!(preserved(&result), before);
        assert_eq!(result.config.model, "gpt-4.1");
        assert!(result.pending_config.is_none());
        assert_eq!(result.run_history.back().unwrap().reason, "complete");
        assert_eq!(*client.round.lock().unwrap(), 3);
    }
}

#[tokio::test]
async fn an_expired_continuation_preserves_the_task_before_work_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = cancelled(dir.path()).await;
    s.config.run_timeout_secs = 1;
    let before = preserved(&s);
    s.receive_message("계속".into()).unwrap();
    let client = Arc::new(Script {
        replies: Mutex::new(VecDeque::new()),
        requests: Mutex::new(vec![]),
        retry_timeouts: Mutex::new(vec![]),
    });
    let (tx, _rx) = mpsc::channel(1);
    tx.send(AgentEvent::Notice {
        session: s.id.clone(),
        text: "blocked event queue".into(),
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        run_session(s, client.clone(), CancellationToken::new(), tx),
    )
    .await
    .expect("the expired snapshot must not start work");
    assert_eq!(preserved(&result), before);
    assert!(client.requests.lock().unwrap().is_empty());
    assert_eq!(
        result.run_history.back().unwrap().workflow,
        "message_routing"
    );
    assert_eq!(result.run_history.back().unwrap().reason, "run_timeout");
}
