use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, Usage},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

enum Reply {
    Complete,
    Exhaust,
    Wait,
    Cancel,
    Fail,
}

#[async_trait]
impl LlmClient for Reply {
    async fn complete(
        &self,
        _: Value,
        config: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        match self {
            Self::Wait => std::future::pending().await,
            Self::Cancel => {
                cancel.cancel();
                anyhow::bail!("cancelled")
            }
            Self::Fail => anyhow::bail!("provider_stream_error: fixture failure"),
            Self::Complete | Self::Exhaust => Ok(Completion {
                text: "Saved answer".into(),
                length_limited: matches!(self, Self::Exhaust),
                usage: Some(Usage {
                    input: if matches!(self, Self::Exhaust) {
                        config.run_tokens
                    } else {
                        123
                    },
                    output: 7,
                    cached: None,
                }),
                ..Default::default()
            }),
        }
    }
}

fn fixture(path: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: path.into(),
            output: path.join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            source_answer_review: false,
            source_document_review: false,
            completion_review_enabled: false,
            ..Default::default()
        },
    );
    s.add_user("First request".into());
    s
}

async fn execute(s: Session, reply: Reply) -> Session {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, Arc::new(reply), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

#[tokio::test]
async fn later_questions_keep_errors_and_usage_is_per_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = execute(fixture(dir.path()), Reply::Fail).await;
    assert_eq!(s.run_history.len(), 1);
    let failed = serde_json::to_value(&s.run_history[0]).unwrap();
    assert_eq!(failed["reason"], "provider_stream_error");
    assert_eq!(failed["error"], "provider_stream_error: fixture failure");
    assert_eq!(failed["last_stage"], "model");
    assert_eq!(failed["usage_estimated"], true);
    s.add_user("Why did it stop?".into());
    let s = execute(s, Reply::Complete).await;
    assert_eq!(s.run_history.len(), 2);
    assert_eq!(serde_json::to_value(&s.run_history[0]).unwrap(), failed);
    let current = &s.run_history[1];
    assert_eq!(s.status, "complete");
    assert!(s.last_error.is_none());
    assert_eq!(current.status, "complete");
    assert_eq!(current.reason, "complete");
    assert_eq!(current.request, "Why did it stop?");
    assert_eq!(current.input_tokens, 123);
    assert_eq!(current.output_tokens, 7);
    assert_eq!(current.rounds, 1);
    assert!(!current.usage_estimated); // Earlier estimated usage is not inherited.
    assert_ne!(current.id, s.run_history[0].id);
    assert!(current.ended_at >= current.started_at);
}

#[tokio::test]
async fn budget_stops_record_the_cause_even_when_a_saved_document_is_finalized() {
    for document in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fixture(dir.path());
        if document {
            s.select_workflow("document_edit").unwrap();
            tools::execute(
                &mut s,
                "document_edit",
                json!({"action":"create","text":"# Result\nSaved draft.\n"}),
            )
            .unwrap();
        }
        let s = execute(s, Reply::Exhaust).await;
        let record = s.run_history.back().unwrap();
        assert_eq!(record.reason, "run_budget_exhausted");
        assert_eq!(record.status, s.status);
        assert_eq!(record.input_tokens, s.config.run_tokens);
        assert_eq!(record.output_tokens, 7);
        assert_eq!(record.last_stage, "continuing");
        if document {
            assert!(matches!(
                s.status.as_str(),
                "complete" | "complete_with_gaps"
            ));
            assert!(record.error.is_none());
        } else {
            assert_eq!(record.status, "blocked");
            assert!(
                record
                    .error
                    .as_ref()
                    .unwrap()
                    .starts_with("run_budget_exhausted")
            );
        }
    }
}

#[tokio::test]
async fn cancellation_and_timeouts_have_distinct_records() {
    for (reply, status, reason) in [
        (Reply::Cancel, "cancelled", "cancelled"),
        (Reply::Wait, "blocked", "run_timeout"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fixture(dir.path());
        s.config.run_timeout_secs = 1;
        let s = execute(s, reply).await;
        let record = s.run_history.back().unwrap();
        assert_eq!(record.status, status);
        assert_eq!(record.reason, reason);
        assert_eq!(record.last_stage, "model");
        assert_eq!(record.rounds, 1);
        assert!(record.usage_estimated);
        assert_eq!(record.timeout_secs, 1);
    }
}
