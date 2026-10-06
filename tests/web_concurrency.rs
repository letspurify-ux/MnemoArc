use crate::support;
use async_trait::async_trait;
use mnemoarc::{
    config::{Config, Project},
    llm::{Completion, LlmClient},
    web::{self, WebState},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct Controlled {
    finish: BTreeMap<String, Notify>,
}

#[async_trait]
impl LlmClient for Controlled {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let label = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .filter_map(|message| message["content"].as_str())
            .find(|text| self.finish.contains_key(*text))
            .unwrap();
        delta.send(format!("stream {label}")).await.unwrap();
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            _ = self.finish[label].notified() => Ok(Completion { text: format!("done {label}"), ..Default::default() }),
        }
    }
}

struct App {
    _dir: tempfile::TempDir,
    state: WebState,
    server: tokio::task::JoinHandle<()>,
    url: String,
    client: reqwest::Client,
    llm: Arc<Controlled>,
    ids: Vec<String>,
}

impl App {
    async fn new(limit: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let project = Project {
            root: dir.path().into(),
            ..Default::default()
        };
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            max_concurrent_sessions: limit,
            completion_review_enabled: false,
            projects: vec![project.clone()],
            ..support::compact_config()
        };
        let llm = Arc::new(Controlled {
            finish: ["first", "second", "third"]
                .into_iter()
                .map(|key| (key.into(), Notify::new()))
                .collect(),
        });
        let state = WebState::new(config, dir.path().join("config.toml"), llm.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = web::router(state.clone(), dir.path().into());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut app = Self {
            _dir: dir,
            state,
            server,
            url,
            client: reqwest::Client::new(),
            llm,
            ids: vec![],
        };
        for _ in 0..3 {
            let added = app
                .post("/api/sessions", json!({"project": project}))
                .await
                .json::<Value>()
                .await
                .unwrap();
            app.ids.push(added["id"].as_str().unwrap().into());
        }
        app
    }
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, data: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header("x-mnemoarc-client", "web")
            .json(&data)
            .send()
            .await
            .unwrap()
    }
    async fn run(&self, index: usize, text: &str) -> reqwest::Response {
        self.post(
            &format!("/api/sessions/{}/run", self.ids[index]),
            json!({"action":"chat", "text":text}),
        )
        .await
    }
    async fn wait(&self, index: usize, field: &str, expected: Value) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let session = self
                    .get(&format!("/api/sessions/{}", self.ids[index]))
                    .await;
                if session[field] == expected {
                    let settled = field != "status"
                        || !self.get("/api/state").await["running"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|run| run["id"] == self.ids[index]);
                    if settled {
                        return session;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn stop(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.state.shutdown())
            .await
            .unwrap();
        self.server.abort();
    }
}

#[tokio::test]
async fn parallel_streams_cancel_and_completion_keep_other_sessions_running() {
    let app = App::new(2).await;
    let (first, second) = tokio::join!(app.run(0, "first"), app.run(1, "second"));
    assert_eq!(first.status(), 200);
    assert_eq!(second.status(), 200);
    let a = app.wait(0, "stream", json!("stream first")).await;
    let b = app.wait(1, "stream", json!("stream second")).await;
    assert_eq!(a["bundles"][0]["messages"][0]["content"], "first");
    assert_eq!(b["bundles"][0]["messages"][0]["content"], "second");
    assert_eq!(
        app.get("/api/state").await["running"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(app.run(0, "replacement").await.status(), 409);
    assert_eq!(app.run(2, "third").await.status(), 409);
    assert!(
        app.get(&format!("/api/sessions/{}", app.ids[2])).await["bundles"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    app.post(&format!("/api/sessions/{}/cancel", app.ids[0]), json!({}))
        .await
        .error_for_status()
        .unwrap();
    app.wait(0, "status", json!("cancelled")).await;
    assert_eq!(
        app.get(&format!("/api/sessions/{}", app.ids[1])).await["stream"],
        "stream second"
    );
    assert_eq!(app.run(2, "third").await.status(), 200);
    app.wait(2, "stream", json!("stream third")).await;

    app.llm.finish["second"].notify_one();
    app.wait(1, "status", json!("complete")).await;
    let state = app.get("/api/state").await;
    assert_eq!(state["running"], json!([{"id":app.ids[2],"closing":false}]));
    assert_eq!(
        app.get(&format!("/api/sessions/{}", app.ids[2])).await["stream"],
        "stream third"
    );
    app.stop().await;
}

#[tokio::test]
async fn closing_one_session_and_shutting_down_settle_every_owner() {
    let app = App::new(3).await;
    for (index, label) in ["first", "second", "third"].iter().enumerate() {
        assert_eq!(app.run(index, label).await.status(), 200);
        app.wait(index, "stream", json!(format!("stream {label}")))
            .await;
    }
    app.client
        .delete(format!("{}/api/sessions/{}", app.url, app.ids[0]))
        .header("x-mnemoarc-client", "web")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while app.get("/api/state").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == app.ids[0])
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state = app.get("/api/state").await;
    assert_eq!(state["running"].as_array().unwrap().len(), 2);
    assert!(
        state["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["status"] == "running")
    );
    tokio::time::timeout(Duration::from_secs(3), app.state.shutdown())
        .await
        .unwrap();
    let state = app.get("/api/state").await;
    assert_eq!(state["running"], json!([]));
    for session in state["sessions"].as_array().unwrap() {
        assert_eq!(session["status"], "cancelled");
    }
    assert_eq!(app.run(1, "second").await.status(), 503);
    app.server.abort();
}

#[tokio::test]
async fn simultaneous_admission_respects_the_limit_and_lowering_it_keeps_running_work() {
    let app = App::new(1).await;
    let (first, second) = tokio::join!(app.run(0, "first"), app.run(1, "second"));
    let mut statuses = [first.status().as_u16(), second.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    let state = app.get("/api/state").await;
    let mut config = state["config"].clone();
    config["max_concurrent_sessions"] = json!(3);
    let save = |config: Value| {
        app.client
            .put(format!("{}/api/settings", app.url))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"config":config}))
            .send()
    };
    save(config.clone())
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let rejected_index = if state["running"][0]["id"] == app.ids[0] {
        1
    } else {
        0
    };
    assert_eq!(
        app.run(rejected_index, ["first", "second"][rejected_index])
            .await
            .status(),
        200
    );
    config["max_concurrent_sessions"] = json!(1);
    save(config).await.unwrap().error_for_status().unwrap();
    assert_eq!(
        app.get("/api/state").await["running"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(app.run(2, "third").await.status(), 409);
    app.stop().await;
}

#[tokio::test]
async fn pending_settings_and_tools_are_delivered_only_to_their_session() {
    let app = App::new(2).await;
    for (index, label) in ["first", "second"].iter().enumerate() {
        assert_eq!(app.run(index, label).await.status(), 200);
        app.wait(index, "stream", json!(format!("stream {label}")))
            .await;
    }
    let original = app.get(&format!("/api/sessions/{}", app.ids[1])).await;
    let mut config = original["config"].clone();
    config["request_timeout_secs"] = json!(37);
    config["max_concurrent_sessions"] = json!(1);
    let put = |path: String, body: Value| {
        app.client
            .put(format!("{}{path}", app.url))
            .header("x-mnemoarc-client", "web")
            .json(&body)
            .send()
    };
    put(
        format!("/api/sessions/{}/settings", app.ids[0]),
        json!({"config":config}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    put(
        format!("/api/sessions/{}/tools", app.ids[0]),
        json!({"names":[]}),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(
        put(
            format!("/api/sessions/{}/workflow", app.ids[1]),
            json!({"workflow":"source_document"})
        )
        .await
        .unwrap()
        .status(),
        409
    );
    assert_eq!(
        put(
            format!("/api/sessions/{}/workflow", app.ids[2]),
            json!({"workflow":"source_document"})
        )
        .await
        .unwrap()
        .status(),
        409
    );
    app.post(&format!("/api/sessions/{}/cancel", app.ids[0]), json!({}))
        .await
        .error_for_status()
        .unwrap();
    let changed = app.wait(0, "status", json!("cancelled")).await;
    let applied = if changed["pending_config"].is_null() {
        &changed["config"]
    } else {
        &changed["pending_config"]
    };
    assert_eq!(applied["request_timeout_secs"], 37);
    assert_eq!(applied["max_concurrent_sessions"], 2);
    assert_eq!(changed["active_tools"], json!([]));
    let unaffected = app.get(&format!("/api/sessions/{}", app.ids[1])).await;
    assert_eq!(unaffected["config"], original["config"]);
    assert_eq!(unaffected["active_tools"], original["active_tools"]);
    assert_eq!(unaffected["stream"], "stream second");
    app.stop().await;
}

#[test]
fn concurrency_limit_is_bounded_and_old_configs_get_the_default() {
    assert_eq!(
        serde_json::from_value::<Config>(json!({}))
            .unwrap()
            .max_concurrent_sessions,
        4
    );
    for limit in [0, 33, usize::MAX] {
        assert!(
            Config {
                max_concurrent_sessions: limit,
                ..support::compact_config()
            }
            .validate()
            .is_err()
        );
    }
    for limit in [1, 4, 32] {
        assert!(
            Config {
                max_concurrent_sessions: limit,
                ..support::compact_config()
            }
            .validate()
            .is_ok()
        );
    }
}
