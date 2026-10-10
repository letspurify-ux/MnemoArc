use crate::support;
use async_trait::async_trait;
use mnemoarc::{
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    tools,
    web::{self, WebState},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const ORIGINAL: &str = "sample.rs 소스로 첫 장과 둘째 장을 작성해줘. 원본은 변경하지 마.";
const CHANGE: &str = "첫 장만 남기도록 문서를 수정해줘.";
const DOCUMENT: &str = "# 첫 장\ncount 함수는 7을 반환한다. sample.rs:1-1\n";

struct Engine {
    output: std::path::PathBuf,
    routing_attempts: Mutex<usize>,
    initial_calls: Mutex<usize>,
    work_calls: Mutex<usize>,
}
fn call(id: &str, name: &str, arguments: Value) -> Completion {
    Completion {
        calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
        }],
        ..Default::default()
    }
}
#[async_trait]
impl LlmClient for Engine {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if payload["session_message_routing"] == true {
            let mut attempts = self.routing_attempts.lock().unwrap();
            *attempts += 1;
            assert_eq!(request["response_format"]["json_schema"]["strict"], true);
            assert_eq!(payload["current_message"], CHANGE);
            if *attempts == 1 {
                return Ok(Completion {
                    text: "첫 장만 작성하겠습니다.".into(),
                    ..Default::default()
                });
            }
            assert!(payload["program_feedback"].is_string());
            return Ok(Completion {text:json!({"intent":"work","authorization_quote":"문서를 수정해줘","changes":{"goal":"sample.rs 소스로 첫 장만 작성한다. 원본은 변경하지 않는다.","completion":["첫 장만 작성"]}}).to_string(),..Default::default()});
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        if state["latest_request"] == ORIGINAL {
            let first = {
                let mut calls = self.initial_calls.lock().unwrap();
                *calls += 1;
                *calls == 1
            };
            if first {
                return Ok(call(
                    "initial-draft",
                    "document_edit",
                    json!({"action":"create","text":"# Draft\nSaved draft.\n"}),
                ));
            }
            cancel.cancelled().await;
            anyhow::bail!("cancelled");
        }
        assert!(
            state["latest_request"]
                .as_str()
                .unwrap()
                .contains("첫 장만")
        );
        let step = {
            let mut calls = self.work_calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        Ok(match step {
            1 => call(
                "read-source",
                "file_read",
                json!({"path":"sample.rs","start_line":1,"max_lines":1}),
            ),
            2 => call(
                "amended-document",
                "document_edit",
                json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&self.output)?),"text":DOCUMENT}),
            ),
            // The first final is sent back once to compare the document
            // with the request; the second confirms it.
            3 | 4 => Completion {
                text: "첫 장을 저장하고 소스와 대조했습니다.".into(),
                ..Default::default()
            },
            _ => anyhow::bail!("unexpected_document_recovery: {step}"),
        })
    }
}
async fn get(client: &reqwest::Client, url: &str) -> Value {
    client
        .get(url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn wait_idle(client: &reqwest::Client, url: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if get(client, &format!("{url}/api/state")).await["running"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_then_sending_a_changed_prompt_repairs_routing_and_finishes_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let source = "pub fn count() -> usize { 7 }\n";
    std::fs::write(dir.path().join("sample.rs"), source).unwrap();
    let project = Project {
        root: dir.path().into(),
        output: dir.path().join("out.md"),
        ..Default::default()
    };
    let engine = Arc::new(Engine {
        output: project.output.clone(),
        routing_attempts: Mutex::new(0),
        initial_calls: Mutex::new(0),
        work_calls: Mutex::new(0),
    });
    let config = Config {
        model: "gpt-4o".into(),
        model_context: Some(128000),
        // The scripted engine does not answer checkpoint cleanup. The default
        // 64K context leaves ~16.8K input budget, and the fixed prompt alone
        // is ~12K, so five rounds crossed high_water and started a checkpoint.
        context_tokens: 128000,
        projects: vec![project.clone()],
        ..support::compact_config()
    };
    let state = WebState::new(config, dir.path().join("config.toml"), engine.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(state.clone(), dir.path().into());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    assert_eq!(
        get(&client, &format!("{url}/api/state")).await["api_version"],
        2
    );
    let created: Value = client
        .post(format!("{url}/api/sessions"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"project":project,"workflow":"source_document"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    let run_url = format!("{url}/api/sessions/{id}/run");
    client
        .post(&run_url)
        .header("x-mnemoarc-client", "web")
        .json(&json!({"action":"message","text":ORIGINAL}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while *engine.initial_calls.lock().unwrap() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    client
        .post(format!("{url}/api/sessions/{id}/cancel"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_idle(&client, &url).await;
    let paused = get(&client, &format!("{url}/api/sessions/{id}")).await;
    assert_eq!(paused["status"], "cancelled");
    assert_eq!(
        std::fs::read_to_string(&project.output).unwrap(),
        "# Draft\nSaved draft.\n"
    );
    client
        .post(&run_url)
        .header("x-mnemoarc-client", "web")
        .json(&json!({"action":"message","text":CHANGE}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_idle(&client, &url).await;
    let finished = get(&client, &format!("{url}/api/sessions/{id}")).await;
    assert_eq!(finished["status"], "complete", "{}", finished["error"]);
    assert_eq!(finished["original_request"], ORIGINAL);
    assert_eq!(finished["task_amendments"].as_array().unwrap().len(), 1);
    assert_eq!(*engine.routing_attempts.lock().unwrap(), 2);
    assert_eq!(finished["run_history"][0]["reason"], "cancelled");
    assert_eq!(finished["run_history"][1]["workflow"], "source_document");
    assert_eq!(std::fs::read_to_string(&project.output).unwrap(), DOCUMENT);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sample.rs")).unwrap(),
        source
    );
    state.shutdown().await;
    server.abort();
}
