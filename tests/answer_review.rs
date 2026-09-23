use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn choose(flag: bool) -> bool {\n if flag { return true; }\n false\n}\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        },
    );
    s.add_user("Explain conditional behavior with citations as JSON".into());
    (dir, s)
}
fn delivered(s: &mut Session) {
    let call = ToolCall {
        id: "read".into(),
        name: "file_read".into(),
        arguments: json!({"path":"a.rs","start_line":1,"max_lines":4}).to_string(),
    };
    let result = tools::run_call(s, &call);
    tools::record_delivered_read(s, &call, &result);
    s.history.push(
        vec![json!({"role":"tool","tool_call_id":"read","content":result.to_string()})],
        true,
    );
}

#[test]
fn review_uses_current_delivered_evidence_and_rejects_stale_or_unread_citations() {
    let (dir, mut s) = setup();
    delivered(&mut s);
    assert!(tools::answer_review::eligible(&s));
    let answer = r#"{"citations":[{"path":"a.rs","start":1,"end":4}]}"#;
    assert!(tools::answer_review::citation_issues(&s, answer).is_empty());
    s.answer_draft = Some(answer.into());
    let request = tools::answer_review::request(&s).unwrap();
    assert!(request.get("tools").is_none());
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        payload["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["numbered_text"].as_str().unwrap().contains("2| if flag"))
    );
    std::fs::write(dir.path().join("a.rs"), "fn changed() {}\n").unwrap();
    assert!(!tools::answer_review::citation_issues(&s, answer).is_empty());
    let request = tools::answer_review::request(&s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["evidence"], json!([]));
    assert_eq!(payload["evidence_omitted"], true);
    s.add_user("Summarize a document".into());
    assert!(!tools::answer_review::eligible(&s));
    assert!(s.answer_draft.is_none());
}

struct Script {
    step: Mutex<usize>,
    bad_citation: bool,
    limited_draft: bool,
}
#[async_trait]
impl LlmClient for Script {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.step.lock().unwrap();
        let answer = match *step {
            0 => Completion {
                calls: vec![ToolCall {
                    id: "r".into(),
                    name: "file_read".into(),
                    arguments: json!({"path":"a.rs","start_line":1,"max_lines":4}).to_string(),
                }],
                ..Default::default()
            },
            1 => {
                let text =
                    r#"{"always_true":true,"citations":[{"path":"a.rs","start":1,"end":4}]}"#;
                delta.try_send(text.into()).unwrap();
                Completion {
                    text: text.into(),
                    length_limited: self.limited_draft,
                    ..Default::default()
                }
            }
            2 => {
                let payload: Value =
                    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap())
                        .unwrap();
                assert_eq!(payload["source_answer_review"], true);
                assert!(request.get("tools").is_none());
                assert!(payload["draft"].as_str().unwrap().contains("true"));
                assert!(!payload["evidence"].as_array().unwrap().is_empty());
                Completion {text:json!({"always_true":false,"citations":[{"path":"a.rs","start":1,"end":if self.bad_citation {50} else {4}}]}).to_string(),..Default::default()}
            }
            _ => panic!("review loop exceeded one call"),
        };
        *step += 1;
        Ok(answer)
    }
}
#[tokio::test]
async fn one_review_replaces_draft_without_streaming_it_and_checks_final_citations() {
    for (bad_citation, limited_draft, with_plan) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, false, true),
    ] {
        let (_dir, mut s) = setup();
        if with_plan {
            tools::execute(
                &mut s,
                "task_plan",
                json!({"action":"apply","expected_revision":0,"operations":[
                    {"op":"insert","texts":["Prepare the review fixture"]},
                    {"op":"complete","id":"T1","result":"Created the source fixture"}
                ]}),
            )
            .unwrap();
        }
        let client = Arc::new(Script {
            step: Mutex::new(0),
            bad_citation,
            limited_draft,
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move {
            let mut text = String::new();
            while let Some(event) = rx.recv().await {
                if let AgentEvent::Delta { text: delta, .. } = event {
                    text.push_str(&delta);
                }
            }
            text
        });
        let s = agent::run_session(s, client.clone(), CancellationToken::new(), tx).await;
        assert_eq!(*client.step.lock().unwrap(), 3, "{:?}", s.last_error);
        assert_eq!(s.status, if bad_citation { "partial" } else { "complete" });
        assert!(s.answer_reviewed);
        let shown = drain.await.unwrap();
        assert!(!shown.contains("\"always_true\":true"));
        assert_eq!(shown.matches("\"always_true\":false").count(), 1);
        assert!(s.answer_draft.is_none());
        if bad_citation {
            assert!(s.last_error.unwrap().contains("answer_citation_check"));
        }
    }
}

#[test]
fn review_is_bounded_and_new_requests_reset_but_resume_preserves_pending_draft() {
    let (_dir, mut s) = setup();
    delivered(&mut s);
    s.answer_draft = Some("draft".into());
    let original = s.answer_review_question.clone();
    s.add_user("resume".into());
    assert_eq!(s.answer_draft.as_deref(), Some("draft"));
    assert_eq!(s.answer_review_question, original);
    s.answer_draft = Some("many words ".repeat(20_000));
    assert!(
        tools::answer_review::request(&s)
            .unwrap_err()
            .to_string()
            .contains("answer_review_budget")
    );
    s.add_user("A new task".into());
    assert!(s.answer_draft.is_none());
    assert!(!s.answer_reviewed);
    assert!(!tools::answer_review::eligible(&s));
}

struct ForbiddenReviewer;
#[async_trait]
impl LlmClient for ForbiddenReviewer {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        Ok(Completion {
            calls: vec![ToolCall {
                id: "write".into(),
                name: "document_edit".into(),
                arguments: json!({"action":"create","text":"must not be written"}).to_string(),
            }],
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn review_cannot_execute_tools_and_budget_exhaustion_retains_draft() {
    let (dir, mut s) = setup();
    s.project.output = dir.path().join("out.md");
    delivered(&mut s);
    s.answer_draft = Some("draft".into());
    s.active_tools.insert("document_edit".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let s = agent::run_session(s, Arc::new(ForbiddenReviewer), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .contains("answer_review_incomplete")
    );
    assert_eq!(s.answer_draft.as_deref(), Some("draft"));
    assert!(!dir.path().join("out.md").exists());
    let mut s = s;
    s.config.run_tokens = 1;
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let s = agent::run_session(s, Arc::new(ForbiddenReviewer), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert!(s.last_error.as_deref().unwrap().contains("run_budget"));
    assert_eq!(s.answer_draft.as_deref(), Some("draft"));
}

#[test]
fn complete_search_lines_are_evidence_but_truncated_hits_are_not() {
    let (_dir, mut s) = setup();
    let call = ToolCall {
        id: "search".into(),
        name: "source_search".into(),
        arguments: json!({"path":"a.rs","query":"if flag"}).to_string(),
    };
    let mut result = tools::run_call(&mut s, &call);
    let answer = r#"{"citations":[{"path":"a.rs","start":2,"end":2}]}"#;
    s.history.push(
        vec![json!({"role":"tool","content":result.to_string()})],
        true,
    );
    assert!(tools::answer_review::citation_issues(&s, answer).is_empty());
    s.history.bundles.pop_back();
    result["data"]["matches"][0]["truncated"] = json!(true);
    s.history.push(
        vec![json!({"role":"tool","content":result.to_string()})],
        true,
    );
    assert!(!tools::answer_review::citation_issues(&s, answer).is_empty());
}

struct EmptyThenValidReview(Mutex<usize>);
#[async_trait]
impl LlmClient for EmptyThenValidReview {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut calls = self.0.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            return Ok(Completion::default());
        }
        assert!(
            request["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("answer_review_incomplete")
        );
        Ok(Completion {
            text: "The false branch returns false (a.rs:1-4).".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn incomplete_answer_review_can_recover_without_losing_draft() {
    let (_dir, mut s) = setup();
    delivered(&mut s);
    s.answer_draft = Some("The false branch returns false (a.rs:1-4).".into());
    let model = Arc::new(EmptyThenValidReview(Mutex::new(0)));
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = agent::run_session(s, model.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(*model.0.lock().unwrap(), 2);
    assert!(result.answer_reviewed);
}
