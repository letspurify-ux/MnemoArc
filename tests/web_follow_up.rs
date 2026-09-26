use async_trait::async_trait;
use mnemoarc::{
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    web::{self, WebState},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Script;
#[async_trait]
impl LlmClient for Script {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        if request["messages"][1]["content"]
            .as_str()
            .unwrap_or("")
            .starts_with("Saved task snapshot:")
        {
            assert!(request.get("tools").is_none());
            return Ok(Completion {
                text: "The original task is still blocked; its repair plan is retained.".into(),
                ..Default::default()
            });
        }
        let raw = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        let state: Value = serde_json::from_str(raw.split_once('\n').unwrap().1).unwrap();
        if state["latest_request"] == "Original task" {
            if state["task"]["plan_revision"] == 0 {
                return Ok(Completion { calls:vec![ToolCall { id:"plan".into(), name:"task_plan".into(), arguments:json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Repair original diagram"]}]}).to_string() }], ..Default::default() });
            }
            anyhow::bail!("provider_error: original task stopped");
        }
        assert_eq!(state["latest_request"], "New task");
        assert_eq!(state["task"]["plan_revision"], 0);
        Ok(Completion {
            text: "New task complete".into(),
            ..Default::default()
        })
    }
}

async fn get(c: &reqwest::Client, url: &str) -> Value {
    c.get(url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn execute(c: &reqwest::Client, url: &str, id: &str, body: Value) -> Value {
    c.post(format!("{url}/api/sessions/{id}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !get(c, &format!("{url}/api/state")).await["running"].is_null() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    get(c, &format!("{url}/api/sessions/{id}")).await
}

#[tokio::test]
async fn default_follow_up_preserves_task_and_explicit_new_task_resets_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        model: "gpt-4o".into(),
        model_context: Some(128000),
        source_answer_review: false,
        completion_review_enabled: false,
        projects: vec![Project {
            root: dir.path().into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = WebState::new(config, dir.path().join("config.toml"), Arc::new(Script)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(state.clone(), dir.path().into());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::new();
    let initial = get(&c, &format!("{url}/api/state")).await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let original = execute(&c, &url, id, json!({"text":"Original task"})).await;
    assert_eq!(original["status"], "blocked");
    assert_eq!(
        original["task"]["todos"][0]["text"],
        "Repair original diagram"
    );
    let question = execute(&c, &url, id, json!({"text":"Why did it stop?"})).await;
    for key in [
        "task",
        "document_review",
        "completion_review",
        "checkpoint",
        "status",
        "error",
        "run_guidance",
    ] {
        assert_eq!(question[key], original[key], "{key}");
    }
    assert_eq!(question["run_history"][1]["status"], "complete");
    assert_eq!(question["run_history"][1]["workflow"], "follow_up");
    assert_eq!(question["run_history"][0], original["run_history"][0]);
    let resumed = execute(&c, &url, id, json!({"text":"계속"})).await;
    assert_eq!(resumed["status"], "blocked");
    assert_eq!(resumed["task"]["todos"], original["task"]["todos"]);
    let new = execute(&c, &url, id, json!({"action":"chat","text":"New task"})).await;
    assert_eq!(new["status"], "complete");
    assert_eq!(new["task"]["todos"], json!([]));
    assert_eq!(new["run_history"].as_array().unwrap().len(), 4);
    state.shutdown().await;
    server.abort();
}
