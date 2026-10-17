use crate::support;
use async_trait::async_trait;
use chrono::{DateTime, Duration as Days, Utc};
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
    dir: Arc<tempfile::TempDir>,
    config: Config,
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
        let dir = Arc::new(tempfile::tempdir().unwrap());
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
        Self::start(dir, config).await
    }
    /// Another app process started with the same settings folder.
    async fn beside(&self) -> Self {
        Self::start(self.dir.clone(), self.config.clone()).await
    }
    /// Another app process whose settings in memory differ.
    async fn beside_with(&self, edit: impl FnOnce(&mut Config)) -> Self {
        let mut config = self.config.clone();
        edit(&mut config);
        Self::start(self.dir.clone(), config).await
    }
    async fn start(dir: Arc<tempfile::TempDir>, config: Config) -> Self {
        let llm = Arc::new(Recorder::default());
        let state =
            WebState::new(config.clone(), dir.path().join("config.toml"), llm.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = web::router(state.clone(), dir.path().into());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            dir,
            config,
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
    /// Creates a session for `project` and runs a message to its end.
    async fn run_user(&self, project: Value) {
        let created: Value = self
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
        let started = self
            .send(
                reqwest::Method::POST,
                &format!("/api/sessions/{id}/run"),
                json!({"text":"stop"}),
            )
            .await;
        assert_eq!(started.status(), 200);
        self.settled().await;
    }
    /// Waits until the schedule file beside the settings satisfies `ready`
    /// and returns its text.
    async fn saved_file(&self, ready: impl Fn(&Value) -> bool) -> String {
        let file = self.dir.path().join("config.automation.json");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&file)
                    && serde_json::from_str::<Value>(&text).is_ok_and(|saved| ready(&saved))
                {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    /// Waits until the lease file marks activity at `at` (kept to the
    /// millisecond); marks are written on a thread of their own.
    async fn marked(&self, at: chrono::DateTime<Utc>) {
        let lease = self.dir.path().join("config.automation.lock");
        let at = std::time::SystemTime::from(
            chrono::DateTime::from_timestamp_millis(at.timestamp_millis()).unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::fs::metadata(&lease)
                .and_then(|m| m.modified())
                .map_or(true, |marked| marked < at)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
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
    app.saved_file(|saved| saved["runs"].as_array().unwrap().len() == 1)
        .await;

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
    app.marked(later).await;
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

    // The run in progress at `later` was work until idle_minutes after it.
    // An empty project prompt uses the default one.
    assert_eq!(app.state.automation_tick(later).await, None);
    assert!(
        app.state
            .automation_tick(later + Days::minutes(31))
            .await
            .is_some()
    );
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
    // Not idle yet, but it takes the schedule, whose holder writes the file.
    assert_eq!(app.state.automation_tick(Utc::now()).await, None);
    // A new lease file marks no work: switching on adds no idle period.
    let lease = app.dir.path().join("config.automation.lock");
    let marked = std::fs::metadata(&lease).unwrap().modified().unwrap();
    assert!(marked < std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(86_400));
    let saved = app.get("/api/state").await["config"]["projects"][0].clone();
    let saved_id = saved["id"].as_str().unwrap().to_owned();
    let mut unsaved = saved.clone();
    unsaved["id"] = json!("unsaved-project");
    for project in [saved.clone(), unsaved] {
        app.run_user(project).await;
    }
    let projects = app.get("/api/state").await["automation"]["projects"].clone();
    let ids: Vec<_> = projects.as_object().unwrap().keys().collect();
    assert_eq!(ids, vec![saved_id.as_str()]);
    let recorded = projects[&saved_id]["last_run_at"].clone();
    assert!(recorded.is_string());

    // Switched off, a run end is neither recorded nor written.
    let mut config = app.get("/api/state").await["config"].clone();
    config["automation"]["enabled"] = json!(false);
    let switched = app
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config}),
        )
        .await;
    assert_eq!(switched.status(), 200);
    let written = app
        .saved_file(|saved| saved["projects"][&saved_id]["last_run_at"] == recorded)
        .await;
    app.run_user(saved).await;
    assert_eq!(
        app.get("/api/state").await["automation"]["projects"][&saved_id]["last_run_at"],
        recorded
    );
    let file = app.dir.path().join("config.automation.json");
    assert_eq!(std::fs::read_to_string(file).unwrap(), written);
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

#[tokio::test]
async fn one_process_schedules_for_a_settings_folder_two_processes_share() {
    let first = App::new(
        |dir| {
            vec![
                Project {
                    auto_prompt: "hold one".into(),
                    ..project(dir, "one", true)
                },
                Project {
                    auto_prompt: "hold two".into(),
                    ..project(dir, "two", true)
                },
            ]
        },
        4,
    )
    .await;
    let second = first.beside().await;
    let later = Utc::now() + Days::days(8);
    let auto = first.state.automation_tick(later).await.unwrap();
    first.wait_running(1).await;
    let chosen = first.requests()[0].clone();
    // The second process is idle too, but the first holds the schedule.
    assert_eq!(second.state.automation_tick(later).await, None);
    assert!(second.requests().is_empty());

    let closed = first
        .send(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{auto}"),
            json!({}),
        )
        .await;
    assert_eq!(closed.status(), 200);
    first.settled().await;
    first
        .saved_file(|saved| saved["runs"].as_array().unwrap().len() == 1)
        .await;
    // Waiting for the schedule, the second process shows the first's runs.
    assert_eq!(second.state.automation_tick(later).await, None);
    let runs = second.get("/api/state").await["automation"]["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "{runs}");

    // Switched off in the first process's window, the second process, whose
    // settings in memory still say on, follows the file and runs nothing.
    let mut config = first.get("/api/state").await["config"].clone();
    let other = config["projects"]
        .as_array()
        .unwrap()
        .iter()
        .position(|p| p["auto_prompt"] != chosen.as_str())
        .unwrap();
    let mut save = |edit: &dyn Fn(&mut Value)| {
        edit(&mut config);
        first.send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config.clone()}),
        )
    };
    let saved = save(&|c| c["automation"]["enabled"] = json!(false)).await;
    assert_eq!(saved.status(), 200);
    assert_eq!(first.state.automation_tick(later).await, None);
    assert_eq!(second.state.automation_tick(later).await, None);
    assert!(second.requests().is_empty());

    // Switched on again with the other project opted out in the first
    // window: the second process takes the free schedule with the first's
    // attempt, so nothing is left to run, and the first waits.
    let saved = save(&|c| {
        c["automation"]["enabled"] = json!(true);
        c["projects"][other]["auto_improve"] = json!(false);
    })
    .await;
    assert_eq!(saved.status(), 200);
    assert_eq!(second.state.automation_tick(later).await, None);
    assert!(second.requests().is_empty());
    assert_eq!(first.state.automation_tick(later).await, None);
    assert_eq!(first.requests().len(), 1);

    // Opted in again with a new prompt there: the second process uses it.
    let saved = save(&|c| {
        c["projects"][other]["auto_improve"] = json!(true);
        c["projects"][other]["auto_prompt"] = json!("hold changed");
    })
    .await;
    assert_eq!(saved.status(), 200);
    let taken = second.state.automation_tick(later).await.unwrap();
    second.wait_running(1).await;
    assert_eq!(second.requests(), vec!["hold changed".to_owned()]);
    let closed = second
        .send(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{taken}"),
            json!({}),
        )
        .await;
    assert_eq!(closed.status(), 200);
    second.settled().await;
    second
        .saved_file(|saved| saved["runs"].as_array().unwrap().len() == 2)
        .await;
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn work_in_another_process_keeps_the_scheduler_waiting() {
    let first = App::new(|dir| vec![project(dir, "docs", true)], 4).await;
    let second = first.beside().await;
    // The first process takes the schedule; it is not idle yet.
    assert_eq!(first.state.automation_tick(Utc::now()).await, None);
    let saved = second.get("/api/state").await["config"]["projects"][0].clone();
    let created: Value = second
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
    let started = second
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{user}/run"),
            json!({"text":"hold user"}),
        )
        .await;
    assert_eq!(started.status(), 200);
    second.wait_running(1).await;

    // The second process's heartbeat marks its long run for the scheduler.
    // Whole seconds: marks are kept to the millisecond.
    let later = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap() + Days::days(8);
    assert_eq!(second.state.automation_tick(later).await, None);
    first.marked(later).await;
    assert_eq!(
        first.state.automation_tick(later + Days::minutes(1)).await,
        None
    );
    assert!(first.requests().is_empty());
    assert!(
        first
            .state
            .automation_tick(later + Days::minutes(31))
            .await
            .is_some()
    );
    first.settled().await;
    assert_eq!(first.requests(), vec!["default prompt".to_owned()]);

    second
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{user}/cancel"),
            json!({}),
        )
        .await;
    second.settled().await;
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn settings_saved_in_another_window_decide_what_runs() {
    let first = App::new(
        |dir| {
            vec![
                Project {
                    auto_prompt: "older".into(),
                    ..project(dir, "older", true)
                },
                Project {
                    auto_prompt: "newer".into(),
                    ..project(dir, "newer", true)
                },
            ]
        },
        4,
    )
    .await;
    let second = first.beside().await;
    let age = |name: &str, days: u64| {
        let document = first.dir.path().join(name).join("docs/guide.md");
        std::fs::File::options()
            .write(true)
            .open(document)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(days * 86_400))
            .unwrap();
    };
    age("older", 30);
    let added = Project {
        auto_prompt: "added".into(),
        ..project(first.dir.path(), "added", true)
    };
    age("added", 20);
    // In the second window: the stalest project opts out, a project is
    // added and the daily limit is lowered to one.
    let mut config = second.get("/api/state").await["config"].clone();
    for saved in config["projects"].as_array_mut().unwrap() {
        if saved["name"] == "older" {
            saved["auto_improve"] = json!(false);
        }
    }
    config["projects"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&added).unwrap());
    config["automation"]["max_runs_per_day"] = json!(1);
    let saved = second
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config}),
        )
        .await;
    assert_eq!(saved.status(), 200);

    // The first process, whose memory has none of it, runs the added
    // project: the opted-out one does not hold the others up.
    let later = Utc::now() + Days::days(8);
    assert!(first.state.automation_tick(later).await.is_some());
    first.settled().await;
    assert_eq!(first.requests(), vec!["added".to_owned()]);
    // The lowered limit holds the next one back for a day; then the added
    // project, attempted, is not run again.
    assert_eq!(
        first.state.automation_tick(later + Days::hours(1)).await,
        None
    );
    assert!(
        first
            .state
            .automation_tick(later + Days::hours(25))
            .await
            .is_some()
    );
    first.settled().await;
    assert_eq!(
        first.requests(),
        vec!["added".to_owned(), "newer".to_owned()]
    );
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn switching_on_in_a_window_since_closed_still_applies() {
    let first = App::new(|dir| vec![project(dir, "docs", true)], 4).await;
    let second = first.beside().await;
    // The first window switches off; the second switches on and closes.
    let mut config = first.get("/api/state").await["config"].clone();
    config["automation"]["enabled"] = json!(false);
    let saved = first
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config.clone()}),
        )
        .await;
    assert_eq!(saved.status(), 200);
    config["automation"]["enabled"] = json!(true);
    let saved = second
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config}),
        )
        .await;
    assert_eq!(saved.status(), 200);
    second.stop().await;
    drop(second);

    // The first window's settings in memory still say off; the file decides.
    let state = first.get("/api/state").await;
    assert_eq!(state["config"]["automation"]["enabled"], false);
    assert!(
        first
            .state
            .automation_tick(Utc::now() + Days::days(8))
            .await
            .is_some()
    );
    first.settled().await;
    assert_eq!(first.requests(), vec!["default prompt".to_owned()]);
    assert_eq!(first.get("/api/state").await["automation"]["active"], true);
    first.stop().await;
}

#[tokio::test]
async fn a_project_saved_without_an_identity_is_not_scheduled() {
    let app = App::new(|dir| vec![project(dir, "saved", true)], 4).await;
    // Saving writes the project identities to the file.
    let config = app.get("/api/state").await["config"].clone();
    let saved = app
        .send(
            reqwest::Method::PUT,
            "/api/settings",
            json!({"config":config}),
        )
        .await;
    assert_eq!(saved.status(), 200);
    // A project added by hand without one gets another on every read.
    let manual = project(app.dir.path(), "manual", true);
    std::fs::File::options()
        .write(true)
        .open(manual.root.join("docs/guide.md"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(30 * 86_400))
        .unwrap();
    let settings = app.dir.path().join("config.toml");
    let mut text = std::fs::read_to_string(&settings).unwrap();
    text.push_str(&format!(
        "\n[[projects]]\nname = \"manual\"\nroot = {}\noutput = \"docs/guide.md\"\nauto_improve = true\nauto_prompt = \"manual\"\n",
        toml::Value::String(manual.root.display().to_string())
    ));
    std::fs::write(&settings, text).unwrap();

    let later = Utc::now() + Days::days(8);
    assert!(app.state.automation_tick(later).await.is_some());
    app.settled().await;
    assert_eq!(app.requests(), vec!["default prompt".to_owned()]);
    assert_eq!(
        app.state.automation_tick(later + Days::hours(1)).await,
        None
    );
    app.stop().await;
}

#[tokio::test]
async fn a_process_that_cannot_run_leaves_the_schedule_to_one_that_can() {
    let first = App::new(|dir| vec![project(dir, "docs", true)], 4).await;
    // No model in the second window's settings: it cannot start a run.
    let second = first.beside_with(|config| config.model.clear()).await;
    let later = Utc::now() + Days::days(8);
    assert_eq!(second.state.automation_tick(later).await, None);
    assert!(first.state.automation_tick(later).await.is_some());
    first.settled().await;
    assert_eq!(first.requests(), vec!["default prompt".to_owned()]);
    assert!(second.requests().is_empty());
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn a_settings_change_keeping_the_file_size_and_time_applies() {
    let app = App::new(
        |dir| vec![project(dir, "one", true), project(dir, "two", true)],
        4,
    )
    .await;
    let mut config = app.get("/api/state").await["config"].clone();
    config["automation"]["max_runs_per_day"] = json!(2);
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

    // Lowered to one in another window within the same second: the file
    // keeps its size and, on coarse file systems, its time.
    let settings = app.dir.path().join("config.toml");
    let modified = std::fs::metadata(&settings).unwrap().modified().unwrap();
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(text.contains("max_runs_per_day = 2"), "{text}");
    std::fs::write(
        &settings,
        text.replace("max_runs_per_day = 2", "max_runs_per_day = 1"),
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&settings)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        app.state.automation_tick(later + Days::hours(1)).await,
        None
    );
    assert_eq!(app.requests().len(), 1);
    app.stop().await;
}
