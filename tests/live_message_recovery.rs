//! Opt-in live routing check. All files are temporary and the document loop is
//! stopped deliberately after confirming that the amended task reaches it.
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project, Secret},
    llm::{Completion, LlmClient, OpenAiClient},
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct LiveRouting {
    inject_invalid: AtomicBool,
    calls: Mutex<usize>,
}
#[async_trait]
impl LlmClient for LiveRouting {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        if request["response_format"]["json_schema"]["name"] == "session_message_routing" {
            *self.calls.lock().unwrap() += 1;
            if self.inject_invalid.swap(false, Ordering::SeqCst) {
                return Ok(Completion {
                    text: "첫 장으로 변경하겠습니다.".into(),
                    ..Default::default()
                });
            }
            return OpenAiClient.complete(request, config, cancel, delta).await;
        }
        anyhow::bail!("live_document_resume_confirmed: amended task reached the document loop");
    }
}

#[tokio::test]
#[ignore = "uses the configured paid model; requires MNEMOARC_LIVE_TEST=1"]
async fn configured_model_recovers_and_routes_changed_document_requests_after_cancellation() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    config.run_tokens = 100000;
    config.run_timeout_secs = 180;
    config.output_tokens = config.output_tokens.min(8000);
    config.source_document_review = false;
    config.completion_review_enabled = false;
    for prompt in [
        "요청 범위를 첫 장으로 변경하고 첫 장만 문서에 남기도록 수정해줘. 원본 sample.rs는 변경하지 마.",
        "sample.rs를 직접 조사해서 한국어 문서를 작성해줘. 이번에는 첫 장에서 count 함수가 반환하는 값만 설명하고 둘째 장은 빼줘. 원본 파일은 변경하지 마.",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sample.rs"),
            "pub fn count() -> usize { 7 }\n",
        )
        .unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            config.clone(),
        );
        s.select_workflow("source_document").unwrap();
        s.receive_message("sample.rs를 조사해서 count 함수와 다른 동작에 대한 두 장의 한국어 문서를 작성해줘. 원본 파일은 변경하지 마.".into()).unwrap();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"create","text":"# 첫 장\nSaved draft.\n# 둘째 장\nPending draft.\n"}),
        )
        .unwrap();
        s.status = "cancelled".into();
        let original = s.original_request.clone();
        let saved = std::fs::read(&s.project.output).unwrap();
        s.receive_message(prompt.into()).unwrap();
        let client = Arc::new(LiveRouting {
            inject_invalid: AtomicBool::new(true),
            calls: Mutex::new(0),
        });
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
        drain.await.unwrap();
        assert!(
            result
                .last_error
                .as_deref()
                .is_some_and(|error| error.starts_with("live_document_resume_confirmed")),
            "{:?}; {:?}",
            result.last_error,
            result.run_history
        );
        assert_eq!(result.original_request, original);
        assert_eq!(result.task_amendments.len(), 1);
        assert_eq!(result.task_amendments[0].request, prompt);
        assert!(result.task_amendments[0].goal.is_some());
        assert_eq!(
            result.run_history.back().unwrap().workflow,
            "source_document"
        );
        assert_eq!(std::fs::read(&result.project.output).unwrap(), saved);
        eprintln!(
            "model={}, routing_attempts={}, input={}, output={}, amended_document_loop_reached=true",
            config.model,
            client.calls.lock().unwrap(),
            result.input_tokens,
            result.output_tokens
        );
    }
}
