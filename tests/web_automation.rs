use crate::support;
use async_trait::async_trait;
use chrono::{Duration as Days, Utc};
use mnemoarc::{
    config::{AutomationConfig, Config, Project},
    llm::{Completion, LlmClient},
    web::{self, WebState},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, sync::Mutex, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Records each request's first user message. A request whose message
/// contains "hold" waits for cancellation; any other ends the run at once.
#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<String>>,
}

#[async_trait]
impl LlmClient for Recorder {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        cancel: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let text = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .find_map(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        self.requests.lock().unwrap().push(text.clone());
        if text.contains("hold") {
            cancel.cancelled().await;
            anyhow::bail!("cancelled");
        }
        anyhow::bail!("http_401: test provider stops the run")
    }
}

struct App {
    dir: tempfile::TempDir,
    state: WebState,
    server: tokio::task::JoinHandle<()>,
    url: String,
    client: reqwest::Client,
    llm: Arc<Recorder>,
}

fn project(dir: &Path, name: &str, document: bool) -> Project {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("docs")).unwrap();
    if document {
        std::fs::write(root.join("docs/guide.md"), "# Guide\n").unwrap();
    }
    Project {
        name: name.into(),
        root,
        output: "docs/guide.md".into(),
        auto_improve: true,
        ..Default::default()
    }
}

impl App {
    async fn new(projects: impl FnOnce(&Path) -> Vec<Project>, limit: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            max_concurrent_sessions: limit,
            automation: AutomationConfig {
                enabled: true,
                prompt: "default prompt".into(),
                ..Default::default()
            },
            projects: projects(dir.path()),
            ..support::compact_config()
        };
        let llm = Arc::new(Recorder::default());
        let state = WebState::new(config, dir.path().join("config.toml"), llm.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = web::router(state.clone(), dir.path().into());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            dir,
            state,
            server,
            url,
            client: reqwest::Client::new(),
            llm,
        }
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
    async fn send(&self, method: reqwest::Method, path: &str, data: Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.url))
            .header("x-mnemoarc-client", "web")
            .json(&data)
            .send()
            .await
            .unwrap()
    }
    /// Waits until no run is active and returns the workspace state.
    async fn settled(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = self.get("/api/state").await;
                if state["running"].as_array().unwrap().is_empty() {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn wait_running(&self, count: usize) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = self.get("/api/state").await;
                if state["running"].as_array().unwrap().len() == count {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    fn requests(&self) -> Vec<String> {
        self.llm.requests.lock().unwrap().clone()
    }
    async fn stop(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.state.shutdown())
            .await
            .unwrap();
        self.server.abort();
    }
}

#[tokio::test]
async fn an_idle_run_uses_the_project_prompt_logs_it_and_closes_its_session() {
    let app = App::new(
        |dir| {
            vec![Project {
                auto_prompt: "project prompt".into(),
                ..project(dir, "docs", true)
            }]
        },
        4,
    )
    .await;
    let later = Utc::now() + Days::days(8);
    let id = app
        .state
        .automation_tick(later)
        .await
        .expect("stale project starts");
    let state = app.settled().await;
    assert_eq!(app.requests(), vec!["project prompt".to_owned()]);
    assert!(
        !state["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "the automatic session closes when its run ends"
    );
    let runs = state["automation"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["project_name"], "docs");
    assert_eq!(runs[0]["status"], "blocked");
    assert!(runs[0]["error"].as_str().unwrap().contains("http_401"));
    // The run's record is written beside the settings once its slot is released.
    let file = app.dir.path().join("config.automation.json");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let saved = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            if saved.is_some_and(|saved| saved["runs"].as_array().unwrap().len() == 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // The attempt makes the project fresh again until stale_days pass.
    assert_eq!(
        app.state.automation_tick(later + Days::hours(1)).await,
        None
    );
    assert!(
        app.state
            .automation_tick(later + Days::days(8))
            .await
            .is_some()
    );
    app.settled().await;
    assert_eq!(app.requests().len(), 2);
    app.stop().await;
}

#[tokio::test]
async fn automatic_runs_wait_for_idle_time_a_document_and_the_daily_limit() {
    let app = App::new(
        |dir| {
            vec![
                project(dir, "missing", false),
                Project {
                    auto_improve: false,
                    ..project(dir, "opted-out", true)
                },
            ]
        },
        4,
    )
    .await;
    let later = Utc::now() + Days::days(8);
    // No output document, and the other project did not opt in.
    assert_eq!(app.state.automation_tick(later).await, None);

    std::fs::write(app.dir.path().join("missing/docs/guide.md"), "# Guide\n").unwrap();
    // A user run keeps the scheduler waiting, and so does the idle time after it.
    let saved = app.get("/api/state").await["config"]["projects"][0].clone();
    let created: Value = app
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"project":saved}),
        )
        .await
        .json()
        .await
        .unwrap();
    let user = created["id"].as_str().unwrap().to_owned();
    let started = app
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{user}/run"),
            json!({"text":"hold user"}),
        )
        .await;
    assert_eq!(started.status(), 200);
    app.wait_running(1).await;
    assert_eq!(app.state.automation_tick(later).await, None);
    app.send(
        reqwest::Method::POST,
        &format!("/api/sessions/{user}/cancel"),
        json!({}),
    )
    .await;
    app.settled().await;
    assert_eq!(
        app.state
            .automation_tick(Utc::now() + Days::minutes(29))
            .await,
        None
    );

    // An empty project prompt uses the default one.
    assert!(app.state.automation_tick(later).await.is_some());
    app.settled().await;
    assert_eq!(app.requests().last().unwrap(), "default prompt");
    app.stop().await;

    let app = App::new(
        |dir| vec![project(dir, "first", true), project(dir, "second", true)],
        4,
    )
    .await;
    let mut config = app.get("/api/state").await["config"].clone();
    config["automation"]["max_runs_per_day"] = json!(1);
    let saved = app
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config}),
        )
        .await;
    assert_eq!(saved.status(), 200);
    let later = Utc::now() + Days::days(8);
    assert!(app.state.automation_tick(later).await.is_some());
    app.settled().await;
    assert_eq!(
        app.state.automation_tick(later + Days::hours(1)).await,
        None,
        "the second stale project waits for the daily limit"
    );
    assert!(
        app.state
            .automation_tick(later + Days::hours(25))
            .await
            .is_some()
    );
    app.settled().await;
    let names: Vec<_> = app.get("/api/state").await["automation"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["project_name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    app.stop().await;
}

#[tokio::test]
async fn only_configured_projects_keep_work_times() {
    let app = App::new(|dir| vec![project(dir, "saved", true)], 4).await;
    let saved = app.get("/api/state").await["config"]["projects"][0].clone();
    let mut unsaved = saved.clone();
    unsaved["id"] = json!("unsaved-project");
    for project in [saved.clone(), unsaved] {
        let created: Value = app
            .send(
                reqwest::Method::POST,
                "/api/sessions",
                json!({"project":project}),
            )
            .await
            .json()
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();
        let started = app
            .send(
                reqwest::Method::POST,
                &format!("/api/sessions/{id}/run"),
                json!({"text":"stop"}),
            )
            .await;
        assert_eq!(started.status(), 200);
        app.settled().await;
    }
    let projects = app.get("/api/state").await["automation"]["projects"].clone();
    let ids: Vec<_> = projects.as_object().unwrap().keys().collect();
    assert_eq!(ids, vec![saved["id"].as_str().unwrap()]);
    assert!(projects[saved["id"].as_str().unwrap()]["last_run_at"].is_string());
    app.stop().await;
}

#[tokio::test]
async fn a_user_run_starts_beside_an_automatic_run_and_closing_it_logs_the_run() {
    let app = App::new(
        |dir| {
            vec![Project {
                auto_prompt: "hold auto".into(),
                ..project(dir, "docs", true)
            }]
        },
        1,
    )
    .await;
    let auto = app
        .state
        .automation_tick(Utc::now() + Days::days(8))
        .await
        .unwrap();
    let state = app.wait_running(1).await;
    assert_eq!(state["running"][0]["automatic"], true);
    let session = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == auto.as_str())
        .unwrap();
    assert_eq!(session["automatic"], true);
    assert_eq!(session["workflow_mode"], "source_document");

    // The session limit of one does not hold the user's run back.
    let saved = state["config"]["projects"][0].clone();
    let created: Value = app
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"project":saved}),
        )
        .await
        .json()
        .await
        .unwrap();
    let user = created["id"].as_str().unwrap().to_owned();
    let started = app
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{user}/run"),
            json!({"text":"hold user"}),
        )
        .await;
    assert_eq!(started.status(), 200);
    app.wait_running(2).await;

    let closed = app
        .send(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{auto}"),
            json!({}),
        )
        .await;
    assert_eq!(closed.status(), 200);
    let state = app.wait_running(1).await;
    assert_eq!(state["running"][0]["id"], user.as_str());
    let runs = state["automation"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "cancelled");
    assert!(
        !state["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == auto.as_str())
    );
    app.send(
        reqwest::Method::POST,
        &format!("/api/sessions/{user}/cancel"),
        json!({}),
    )
    .await;
    app.settled().await;
    app.stop().await;
}
