use crate::support;
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
        ..support::compact_config()
    };
    launch_config(path, c).await
}
async fn launch_config(
    path: &std::path::Path,
    config: Config,
) -> (String, WebState, tokio::task::JoinHandle<()>) {
    let state = WebState::new(config, path.join("config.toml"), Arc::new(Waiting)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(state.clone(), path.into());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    support::seed_session(&reqwest::Client::new(), &url).await;
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
async fn shared_source_projects_keep_session_ownership_and_requests_separate() {
    let dir = tempfile::tempdir().unwrap();
    let first = Project {
        name: "First project".into(),
        root: dir.path().into(),
        ..Default::default()
    };
    let second = Project {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Second project".into(),
        ..first.clone()
    };
    let config = Config {
        model: "gpt-4o".into(),
        model_context: Some(128000),
        projects: vec![first.clone(), second.clone()],
        ..support::compact_config()
    };
    let (url, state, server) = launch_config(dir.path(), config).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let first_id = initial["sessions"][0]["id"].as_str().unwrap();
    let created = client
        .post(format!("{url}/api/sessions"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"project":second}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let second_id = created["id"].as_str().unwrap();
    assert_ne!(first_id, second_id);

    // Even an empty session cannot be reassigned to another same-folder project.
    let rejected = client
        .put(format!("{url}/api/sessions/{first_id}/project"))
        .header("x-mnemoarc-client", "web")
        .json(&second)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 409);
    assert_eq!(
        get(&client, &url, "/api/state").await["revision"],
        initial["revision"].as_u64().unwrap() + 1
    );

    for (id, text) in [(first_id, "First request"), (second_id, "Second request")] {
        client
            .post(format!("{url}/api/sessions/{id}/run"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"text":text}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    state.shutdown().await;
    for (id, project, text) in [
        (first_id, &first, "First request"),
        (second_id, &second, "Second request"),
    ] {
        let detail = get(&client, &url, &format!("/api/sessions/{id}")).await;
        assert_eq!(detail["project"]["id"], project.id);
        assert_eq!(detail["bundles"][0]["messages"][0]["content"], text);
    }

    client
        .delete(format!("{url}/api/sessions/{first_id}"))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let remaining = get(&client, &url, "/api/state").await;
    assert_eq!(remaining["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["sessions"][0]["id"], second_id);
    assert_eq!(remaining["sessions"][0]["project"]["id"], second.id);
    server.abort();
}

#[tokio::test]
async fn legacy_session_project_requests_preserve_unambiguous_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let mut project = initial["config"]["projects"][0].clone();
    let project_id = project.as_object_mut().unwrap().remove("id").unwrap();
    let created = client
        .post(format!("{url}/api/sessions"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"project":project}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    assert_eq!(
        get(&client, &url, &format!("/api/sessions/{id}")).await["project"]["id"],
        project_id
    );

    project["name"] = json!("Session-specific name");
    client
        .put(format!("{url}/api/sessions/{id}/project"))
        .header("x-mnemoarc-client", "web")
        .json(&project)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let renamed = get(&client, &url, &format!("/api/sessions/{id}")).await;
    assert_eq!(renamed["project"]["id"], project_id);
    assert_eq!(renamed["project"]["name"], "Session-specific name");
    state.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn resume_and_cleanup_reject_missing_task_or_discarded_message() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    for (action, text, expected) in [
        ("resume", "", "재개하거나 정리할 작업이 없습니다"),
        ("cleanup", "", "재개하거나 정리할 작업이 없습니다"),
        ("resume", "새 요구사항", "메시지를 받지 않습니다"),
        ("cleanup", "새 요구사항", "메시지를 받지 않습니다"),
    ] {
        let response = client
            .post(format!("{url}/api/sessions/{id}/run"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"action":action,"text":text}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{action}: {text}");
        let body: Value = response.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains(expected), "{body}");
    }
    let after = get(&client, &url, "/api/state").await;
    assert_eq!(after["revision"], initial["revision"]);
    assert!(after["running"].as_array().unwrap().is_empty());
    assert_eq!(
        get(&client, &url, &format!("/api/sessions/{id}")).await["bundles"],
        json!([])
    );
    state.shutdown().await;
    server.abort();
}
#[tokio::test]
async fn unavailable_saved_projects_do_not_prevent_startup_or_repair() {
    for available in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let missing = Project {
            name: "Moved project".into(),
            root: dir.path().join("missing"),
            ..Default::default()
        };
        let valid = Project {
            name: "Available project".into(),
            root: dir.path().into(),
            ..Default::default()
        };
        let mut projects = vec![missing.clone()];
        if available {
            projects.push(valid.clone());
        }
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            projects,
            ..support::compact_config()
        };
        let (url, state, server) = launch_config(dir.path(), config).await;
        let client = reqwest::Client::new();
        let initial = get(&client, &url, "/api/state").await;
        assert_eq!(initial["config"]["projects"][0]["name"], "Moved project");
        assert_eq!(
            initial["sessions"].as_array().unwrap().len(),
            usize::from(available)
        );
        if available {
            assert_eq!(
                initial["sessions"][0]["project"]["name"],
                "Available project"
            );
        }
        let rejected = client
            .post(format!("{url}/api/sessions"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"project":missing}))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), 400);
        let mut repaired = initial["config"].clone();
        repaired["projects"] = json!([valid]);
        let saved = client
            .put(format!("{url}/api/settings"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"config":repaired}))
            .send()
            .await
            .unwrap();
        assert!(
            saved.status().is_success(),
            "{}",
            saved.text().await.unwrap()
        );
        let created = client
            .post(format!("{url}/api/sessions"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"project":valid}))
            .send()
            .await
            .unwrap();
        assert!(
            created.status().is_success(),
            "{}",
            created.text().await.unwrap()
        );
        assert_eq!(
            get(&client, &url, "/api/state").await["sessions"]
                .as_array()
                .unwrap()
                .len(),
            usize::from(available) + 1
        );
        state.shutdown().await;
        server.abort();
    }
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
async fn session_connection_check_uses_the_sessions_credential() {
    use axum::{Router, http::HeaderMap, http::StatusCode, routing::post};

    let (auth_tx, mut auth_rx) = tokio::sync::mpsc::unbounded_channel();
    let mock = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap| {
            let auth_tx = auth_tx.clone();
            async move {
                let authorization = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let _ = auth_tx.send(authorization);
                (StatusCode::UNAUTHORIZED, "probe stopped")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let mut config = initial["config"].clone();
    config["base_url"] = json!(upstream);
    let global = client
        .put(format!("{url}/api/settings"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":config,"api_key":"global-test-key","credential_mode":"session"}))
        .send()
        .await
        .unwrap();
    assert!(global.status().is_success());
    let session = client
        .put(format!("{url}/api/sessions/{id}/settings"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":config,"api_key":"session-test-key","credential_mode":"session"}))
        .send()
        .await
        .unwrap();
    assert!(session.status().is_success());

    let session_check = client
        .post(format!("{url}/api/sessions/{id}/check"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":config,"credential_mode":"keep"}))
        .send()
        .await
        .unwrap();
    assert_eq!(session_check.status(), 400);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), auth_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "Bearer session-test-key"
    );

    let global_check = client
        .post(format!("{url}/api/check"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"config":config,"credential_mode":"keep"}))
        .send()
        .await
        .unwrap();
    assert_eq!(global_check.status(), 400);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), auth_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "Bearer global-test-key"
    );

    state.shutdown().await;
    server.abort();
    mock_server.abort();
}
#[tokio::test]
async fn session_switch_reconnect_cancel_and_close_preserve_owner() {
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
    let parallel = c
        .post(format!("{url}/api/sessions/{other}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"text":"second"}))
        .send()
        .await
        .unwrap();
    assert_eq!(parallel.status(), 200);
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
        get(&c, &url, &format!("/api/sessions/{other}")).await["bundles"][0]["messages"][0]["content"],
        json!("second")
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
    config["review_limit"] = json!(16);
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
    assert_eq!(
        get(&c, &url, &format!("/api/sessions/{id}")).await["config"]["review_limit"],
        16
    );
    assert_eq!(
        get(&c, &url, "/api/state").await["config"]["review_limit"],
        initial["config"]["review_limit"]
    );
    assert!(!dir.path().join("config.toml").exists());
    state.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn state_details_and_reconnected_events_identify_the_same_server_instance() {
    let dir = tempfile::tempdir().unwrap();
    let mut previous = String::new();
    for _ in 0..2 {
        let (url, state, server) = launch(dir.path()).await;
        let current: Value = reqwest::get(format!("{url}/api/state"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current["api_version"], 2);
        let instance = current["server_instance"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(instance).is_ok());
        assert_ne!(instance, previous);
        let id = current["sessions"][0]["id"].as_str().unwrap();
        let detail: Value = reqwest::get(format!("{url}/api/sessions/{id}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["server_instance"], instance);
        // Each reconnect starts with a full-change event, even without edits.
        let first = reqwest::get(format!("{url}/api/events")).await.unwrap();
        let reconnect = reqwest::get(format!("{url}/api/events")).await.unwrap();
        state.shutdown().await;
        for response in [first, reconnect] {
            let body = tokio::time::timeout(std::time::Duration::from_secs(1), response.text())
                .await
                .unwrap()
                .unwrap();
            let data = body
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let event: Value = serde_json::from_str(data).unwrap();
            assert_eq!(event["server_instance"], instance);
            assert_eq!(event["revision"], 0);
            assert_eq!(event["state"], true);
        }
        previous = instance.to_owned();
        server.abort();
    }
}

#[tokio::test]
async fn shutdown_interrupts_a_pending_connection_probe() {
    use axum::{Router, routing::post};
    use std::time::Duration;
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let mock = Router::new().route(
        "/chat/completions",
        post(move || {
            let signal = signal.clone();
            async move {
                signal.notify_one();
                std::future::pending::<String>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let mut config = get(&client, &url, "/api/state").await["config"].clone();
    config["base_url"] = json!(upstream);
    config["request_timeout_secs"] = json!(60);
    let mut pending = tokio::spawn(async move {
        client
            .post(format!("{url}/api/check"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"config":config}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    state.shutdown().await;
    let result = tokio::time::timeout(Duration::from_secs(1), &mut pending).await;
    pending.abort();
    server.abort();
    mock_server.abort();
    assert_eq!(
        result
            .expect("shutdown left connection probe active")
            .unwrap()
            .status(),
        503
    );
}

#[cfg(unix)]
#[tokio::test]
async fn output_rejects_fifo_without_waiting_for_a_writer() {
    use std::{os::unix::fs::OpenOptionsExt, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docs/source-summary.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        client.get(format!("{url}/api/sessions/{id}/output")).send(),
    )
    .await;
    if result.is_err() {
        // Release the old implementation's blocked reader before failing the
        // regression test; never leave a blocking worker stuck at test exit.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        drop(writer);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    state.shutdown().await;
    server.abort();
    assert_eq!(
        result
            .expect("output API waited for a FIFO writer")
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn output_preview_truncates_at_utf8_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docs/source-summary.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let prefix = "a".repeat(2 * 1024 * 1024 - 1);
    std::fs::write(&path, format!("{prefix}한글")).unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let response = client
        .get(format!("{url}/api/sessions/{id}/output"))
        .send()
        .await
        .unwrap();
    state.shutdown().await;
    server.abort();
    assert!(response.status().is_success());
    let data: Value = response.json().await.unwrap();
    assert_eq!(data["truncated"], true);
    assert_eq!(data["content"], prefix);
}
#[tokio::test]
async fn output_download_returns_the_full_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docs/source-summary.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let content = format!("{}한글 끝", "a".repeat(2 * 1024 * 1024));
    std::fs::write(&path, &content).unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let client = reqwest::Client::new();
    let initial = get(&client, &url, "/api/state").await;
    let id = initial["sessions"][0]["id"].as_str().unwrap();
    let response = client
        .get(format!("{url}/api/sessions/{id}/output/download"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_DISPOSITION],
        "attachment"
    );
    let bytes = response.bytes().await.unwrap();
    state.shutdown().await;
    server.abort();
    assert_eq!(bytes.as_ref(), content.as_bytes());
}
#[tokio::test]
async fn workflow_is_selected_at_creation_and_locked_for_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let (url, state, server) = launch(dir.path()).await;
    let c = reqwest::Client::new();
    let id = get(&c, &url, "/api/state").await["sessions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let select = |workflow: &str| {
        c.put(format!("{url}/api/sessions/{id}/workflow"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"workflow":workflow}))
            .send()
    };
    assert_eq!(
        get(&c, &url, &format!("/api/sessions/{id}")).await["workflow_mode"],
        "answer"
    );
    for invalid in ["", "unknown"] {
        assert_eq!(select(invalid).await.unwrap().status(), 400);
    }
    assert_eq!(select("source_document").await.unwrap().status(), 409);
    assert_eq!(select("answer").await.unwrap().status(), 200);
    let started = c
        .post(format!("{url}/api/sessions/{id}/run"))
        .header("x-mnemoarc-client", "web")
        .json(&json!({"text":"write the manual"}))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 200);
    // The request keeps its creation-time workflow; even an idempotent update
    // is rejected while the session is running.
    let running = get(&c, &url, &format!("/api/sessions/{id}")).await;
    assert_eq!(running["workflow_mode"], "answer");
    assert_eq!(running["task"]["workflow"], "answer");
    assert_eq!(select("answer").await.unwrap().status(), 409);
    c.post(format!("{url}/api/sessions/{id}/cancel"))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap();
    state.shutdown().await;
    server.abort();
}
