use async_trait::async_trait;
use mnemoarc::{
    config::{Config, Project},
    llm::{Completion, LlmClient},
    web::{self, WebState},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
struct Waiting;
#[async_trait]
impl LlmClient for Waiting {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let _ = delta.send("진행 중인 응답".into()).await;
        cancel.cancelled().await;
        anyhow::bail!("cancelled")
    }
}
async fn launch(path: &std::path::Path) -> (String, WebState, tokio::task::JoinHandle<()>) {
    let c = Config {
        model: "gpt-4o".into(),
        model_context: Some(128000),
        projects: vec![Project {
            root: path.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = WebState::new(c, path.join("config.toml"), Arc::new(Waiting)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(state.clone(), path.into());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, state, server)
}
async fn get(client: &reqwest::Client, url: &str, path: &str) -> Value {
    client
        .get(format!("{url}{path}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
#[tokio::test]
async fn settings_are_complete_validated_persisted_and_credentials_never_returned() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let c = reqwest::Client::new();
    let initial = get(&c, &url, "/api/state").await;
    let mut config = initial["config"].clone();
    config["read_parallelism"] = json!(2);
    let res = c
        .put(format!("{url}/api/settings"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":config,"api_key":"test-only-credential","credential_mode":"save"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.text().await.unwrap());
    let data = get(&c, &url, "/api/state").await;
    assert_eq!(data["config"]["read_parallelism"], 2);
    assert_eq!(data["credential"]["saved"], true);
    assert!(!data.to_string().contains("test-only-credential"));
    let saved = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(!saved.contains("test-only-credential"));
    assert!(dir.path().join("config.credentials.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("config.credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let mut invalid = config;
    invalid["context_tokens"] = json!(1);
    let res = c
        .put(format!("{url}/api/settings"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":invalid}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        get(&c, &url, "/api/state").await["config"]["context_tokens"],
        64000
    );
    state.shutdown().await;
    server.abort();
}
#[tokio::test]
async fn session_switch_reconnect_busy_cancel_and_close_preserve_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let c = reqwest::Client::new();
    let data = get(&c, &url, "/api/state").await;
    let id = data["sessions"][0]["id"].as_str().unwrap();
    let started = c
        .post(format!("{url}/api/sessions/{id}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"text":"retain my constraint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 200);
    let created: Value = c
        .post(format!("{url}/api/sessions"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"project":data["config"]["projects"][0]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let other = created["id"].as_str().unwrap();
    let rejected = c
        .post(format!("{url}/api/sessions/{other}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"text":"second"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 409);
    for _ in 0..100 {
        let s = get(&c, &url, &format!("/api/sessions/{id}")).await;
        if s["stream"] == "진행 중인 응답" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let first = get(&c, &url, &format!("/api/sessions/{id}")).await;
    assert_eq!(first["stream"], "진행 중인 응답");
    assert_eq!(
        first["bundles"][0]["messages"][0]["content"],
        "retain my constraint"
    );
    assert_eq!(
        get(&c, &url, &format!("/api/sessions/{other}")).await["bundles"],
        json!([])
    );
    let sse = c.get(format!("{url}/api/events")).send().await.unwrap();
    assert_eq!(sse.headers()["content-type"], "text/event-stream");
    drop(sse);
    c.post(format!("{url}/api/sessions/{id}/cancel"))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap();
    state.shutdown().await;
    let after = get(&c, &url, &format!("/api/sessions/{id}")).await;
    assert_eq!(after["status"], "cancelled");
    assert_eq!(
        after["bundles"][0]["messages"][0]["content"],
        "retain my constraint"
    );
    c.delete(format!("{url}/api/sessions/{id}"))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap();
    assert_eq!(
        c.get(format!("{url}/api/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    server.abort();
}
#[tokio::test]
async fn local_api_rejects_cross_origin_mutation_and_invalid_project() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let c = reqwest::Client::new();
    assert_eq!(
        c.put(format!("{url}/api/settings"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.get(format!("{url}/api/state"))
            .header("origin", "https://example.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.get(format!("{url}/api/state"))
            .header("host", "example.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let initial = get(&c, &url, "/api/state").await;
    let mut project = initial["config"]["projects"][0].clone();
    project["root"] = json!(dir.path().join("missing"));
    assert_eq!(
        c.post(format!("{url}/api/sessions"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"project":project}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let mut config = initial["config"].clone();
    config["run_tokens"] = json!(100000);
    assert_eq!(
        c.put(format!("{url}/api/sessions/{id}/settings"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"config":config}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        get(&c, &url, &format!("/api/sessions/{id}")).await["config"]["run_tokens"],
        100000
    );
    assert_eq!(
        get(&c, &url, "/api/state").await["config"]["run_tokens"],
        500000
    );
    assert!(!dir.path().join("config.toml").exists());
    state.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn shutdown_closes_event_streams_without_waiting_for_browser_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let response = reqwest::get(format!("{url}/api/events")).await.unwrap();
    state.shutdown().await;
    let body = tokio::time::timeout(std::time::Duration::from_secs(1), response.text())
        .await
        .unwrap()
        .unwrap();
    assert!(body.contains("event: changed"));
    server.abort();
}
