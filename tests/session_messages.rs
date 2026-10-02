use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Script(Mutex<Vec<Completion>>, Mutex<Vec<Value>>);
#[async_trait]
impl LlmClient for Script {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        self.1.lock().unwrap().push(request);
        let mut reply = self.0.lock().unwrap().remove(0);
        if let Some(call) = reply.calls.first_mut()
            && call.name == "memory_write"
        {
            let requests = self.1.lock().unwrap();
            let messages = requests.last().unwrap()["messages"].as_array().unwrap();
            let source = messages
                .iter()
                .rev()
                .filter(|m| m["role"] == "tool")
                .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
                .find_map(|m| m["data"]["source"]["id"].as_str().map(str::to_owned))
                .unwrap();
            let mut arguments: Value = serde_json::from_str(&call.arguments).unwrap();
            arguments["source_ids"] = json!([source]);
            call.arguments = arguments.to_string();
        }
        Ok(reply)
    }
}
fn text(value: Value) -> Completion {
    Completion {
        text: value.to_string(),
        ..Default::default()
    }
}
fn route(intent: &str, instruction: &str) -> Completion {
    text(
        json!({"intent":intent,"authorization_quote":instruction,"changes":if intent == "discuss" { Value::Null } else { json!({}) }}),
    )
}
fn call(name: &str, arguments: Value) -> Completion {
    Completion {
        calls: vec![ToolCall {
            id: format!("{name}-id"),
            name: name.into(),
            arguments: arguments.to_string(),
        }],
        ..Default::default()
    }
}
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
            ..Default::default()
        },
    );
    s.receive_message("Create the current report".into())
        .unwrap();
    tools::execute(&mut s,"task_plan",json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Retain this pending step"]}]})).unwrap();
    s.status = "blocked".into();
    s.last_error = Some("run_budget_exhausted: saved work".into());
    s
}
async fn run(s: Session, replies: Vec<Completion>) -> (Session, Vec<Value>) {
    let client = Arc::new(Script(Mutex::new(replies), Mutex::new(vec![])));
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let s = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let requests = client.1.lock().unwrap().clone();
    (s, requests)
}
#[tokio::test]
async fn general_follow_up_collects_files_and_remembers_evidence_without_reviews_or_resuming() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "Newly collected evidence\n").unwrap();
    std::fs::write(dir.path().join("out.md"), "# Saved\nExisting document\n").unwrap();
    let mut s = fixture(dir.path());
    let plan = serde_json::to_value(&s.task).unwrap();
    s.receive_message("Read notes.txt and explain the saved task".into())
        .unwrap();
    let (s,requests)=run(s,vec![route("discuss",""),call("file_read",json!({"path":"notes.txt","start_line":1,"max_lines":1})),call("memory_write",json!({"key":"collected","title":"New evidence","summary":"Collected evidence","kind":"fact","body":"Newly collected evidence","source_ids":[]})),call("document_inspect",json!({"section":"# Saved"})),Completion {text:"Evidence is in notes.txt:1-1".into(),..Default::default()}]).await;
    assert_eq!(s.status, "blocked");
    assert_eq!(serde_json::to_value(&s.task).unwrap(), plan);
    assert_eq!(s.latest_request, "Create the current report");
    assert!(s.sources.values().any(|source| {
        source
            .path
            .as_ref()
            .is_some_and(|p| p.ends_with("notes.txt"))
    }));
    assert!(
        s.memory
            .entries
            .values()
            .any(|memory| memory.key.as_deref() == Some("collected"))
    );
    assert!(
        s.history
            .bundles
            .iter()
            .flat_map(|bundle| bundle.messages.iter())
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| serde_json::from_str::<Value>(message["content"].as_str()?).ok())
            .any(|result| result["status"] == "ok"
                && result["data"]["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("out.md"))
                && result.to_string().contains("Existing document"))
    );
    assert_eq!(s.run_history.back().unwrap().status, "complete");
    assert_eq!(
        s.run_history.back().unwrap().request,
        "Read notes.txt and explain the saved task"
    );
    for request in requests.iter().skip(1) {
        let names: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"file_read")
                && names.contains(&"document_inspect")
                && names.contains(&"memory_write")
        );
        assert!(
            !names.contains(&"investigation")
                && !names.contains(&"document_audit")
                && !names.contains(&"document_edit")
        );
    }
    assert!(!s.document_review.pending && !s.completion_review.pending);
    assert_eq!(s.reviews, 0);
}
#[tokio::test]
async fn further_work_edits_files_and_preserves_the_original_plan_and_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    let revision = s.task.plan_revision;
    tools::execute(&mut s,"task_plan",json!({"action":"apply","expected_revision":revision,"operations":[{"op":"complete","id":"T1","result":"Previously completed work"}]})).unwrap();
    let plan = s.task.todos.clone();
    let sources = s.sources.len();
    s.receive_message("Create extra.txt for this report".into())
        .unwrap();
    let (s, requests) = run(
        s,
        vec![
            route("work", "Create extra.txt"),
            call(
                "file_write",
                json!({"path":"extra.txt","content":"Additional result\n"}),
            ),
            Completion {
                text: "Saved extra.txt".into(),
                ..Default::default()
            },
        ],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("extra.txt")).unwrap(),
        "Additional result\n"
    );
    assert_eq!(s.task.todos.len(), plan.len());
    assert_eq!(s.task.todos[0].id, plan[0].id);
    assert!(s.sources.len() >= sources);
    assert_eq!(s.original_request, "Create the current report");
    assert_eq!(s.latest_request, s.original_request);
    assert_eq!(s.task_amendments.len(), 1);
    assert_eq!(
        s.run_history.back().unwrap().request,
        "Create extra.txt for this report"
    );
    assert_eq!(s.run_history.back().unwrap().workflow, "answer");
    assert!(!requests.iter().any(|r| {
        r["messages"][0]["content"]
            .as_str()
            .unwrap_or("")
            .starts_with("Independently check")
    }));
}
#[tokio::test]
async fn an_explicit_goal_and_completion_change_updates_the_same_task() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    let revision = s.task.plan_revision;
    tools::execute(&mut s,"task_plan",json!({"action":"apply","expected_revision":revision,"operations":[{"op":"complete","id":"T1","result":"Previously completed work"}]})).unwrap();
    s.receive_message("Change the goal and completion condition to the first chapter only".into())
        .unwrap();
    let (s, requests) = run(s, vec![
        text(json!({"intent":"work","authorization_quote":"Change the goal and completion condition","changes":{"goal":"Create a report about the first chapter only","completion":["First chapter only"]}})),
        Completion {text:"Updated the goal and completion condition".into(),..Default::default()},
    ]).await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert_eq!(s.original_request, "Create the current report");
    assert_eq!(
        s.latest_request,
        "Create a report about the first chapter only"
    );
    assert_eq!(s.request_review_criteria.completion, ["First chapter only"]);
    assert_eq!(s.task.completion, s.request_review_criteria.completion);
    assert_eq!(s.task.todos[0].id, "T1");
    assert!(s.task.todos[0].done);
    assert_eq!(s.task_amendments[0].request, s.current_request);
    assert_eq!(requests.len(), 2);
    assert_eq!(s.reviews, 0);
}

#[tokio::test]
async fn discussion_rejects_an_entire_batch_containing_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    let before = serde_json::to_value(&s.task).unwrap();
    s.receive_message("Why did it stop?".into()).unwrap();
    let forbidden = call(
        "file_write",
        json!({"path":"bad.txt","content":"Forbidden"}),
    );
    let (s, _) = run(s, vec![route("discuss", ""), forbidden]).await;
    assert!(!dir.path().join("bad.txt").exists());
    assert_eq!(serde_json::to_value(&s.task).unwrap(), before);
    assert_eq!(s.status, "blocked");
    assert!(
        s.run_history
            .back()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .starts_with("question_tools_not_allowed")
    );
}
#[tokio::test]
async fn routing_cannot_promote_work_from_a_previous_instruction() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = fixture(dir.path());
    s.receive_message("Why did it stop?".into()).unwrap();
    let (s, _) = run(s, vec![route("work", "Create the current report")]).await;
    assert!(s.task_amendments.is_empty());
    assert_eq!(s.latest_request, "Create the current report");
    assert!(
        s.run_history
            .back()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .starts_with("message_routing_invalid")
    );
}
