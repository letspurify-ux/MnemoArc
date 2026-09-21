//! Local browser API. The running agent owns its Session; the web layer holds display snapshots.
use crate::{
    agent::{self, AgentEvent, RunCommand},
    config::{Config, Project, Secret},
    llm::{LlmClient, OpenAiClient},
    session::Session,
    tools::{self, ToolRegistry},
};
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post, put},
};
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

struct Running {
    id: String,
    cancel: CancellationToken,
    commands: mpsc::Sender<RunCommand>,
    closing: bool,
}
struct Core {
    config: Config,
    path: PathBuf,
    sessions: BTreeMap<String, Session>,
    order: Vec<String>,
    streams: BTreeMap<String, String>,
    running: Option<Running>,
    revision: u64,
    credentials: BTreeMap<String, Secret>,
}
#[derive(Clone)]
pub struct WebState {
    core: Arc<Mutex<Core>>,
    events: broadcast::Sender<u64>,
    client: Arc<dyn LlmClient>,
    stopping: CancellationToken,
}
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, e.to_string())
    }
}
type Api<T = Value> = std::result::Result<Json<T>, ApiError>;
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "세션을 찾을 수 없습니다.".into())
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "다른 작업이 실행 중입니다. 작업을 중지한 뒤 다시 시도하세요.".into(),
    )
}
fn changed(s: &WebState, c: &mut Core) {
    c.revision = c.revision.saturating_add(1);
    let _ = s.events.send(c.revision);
}
fn credential_path(path: &FsPath) -> PathBuf {
    path.with_extension("credentials.json")
}
fn write_credentials(path: &FsPath, keys: &BTreeMap<String, Secret>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(FsPath::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    use std::io::Write;
    let values: BTreeMap<_, _> = keys.iter().map(|(k, v)| (k, &v.0)).collect();
    file.write_all(serde_json::to_string(&values)?.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
fn normalize_project(mut project: Project) -> Result<Project> {
    project.root = project.root.canonicalize()?;
    if !project.root.is_dir() || project.name.trim().is_empty() {
        bail!("프로젝트 이름과 실제 폴더가 필요합니다.");
    }
    tools::output_path(&project)?;
    for pattern in project.include.iter().chain(&project.exclude) {
        globset::Glob::new(pattern)?;
    }
    Ok(project)
}
impl WebState {
    pub fn new(mut config: Config, path: PathBuf, client: Arc<dyn LlmClient>) -> Result<Self> {
        let credentials: BTreeMap<String, Secret> = if credential_path(&path).exists() {
            let data: BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(credential_path(&path))?)?;
            data.into_iter().map(|(k, v)| (k, Secret(v))).collect()
        } else {
            BTreeMap::new()
        };
        // A caller may provide a session-only key directly (for example, a
        // headless embedding). A credentials file overrides it when present,
        // but its absence must not erase the supplied in-memory key.
        if let Some(saved) = credentials.get(&config.api_key_env).cloned() {
            config.api_key = Some(saved);
        }
        if config.projects.is_empty() {
            config.projects.push(Project {
                name: "MnemoArc".into(),
                root: std::env::current_dir()?,
                ..Default::default()
            });
        }
        for project in &mut config.projects {
            if let Ok(root) = project.root.canonicalize() {
                project.root = root;
            }
        }
        let session = Session::new(config.projects[0].clone(), config.clone());
        let id = session.id.clone();
        let (events, _) = broadcast::channel(128);
        Ok(Self {
            core: Arc::new(Mutex::new(Core {
                config,
                path,
                sessions: BTreeMap::from([(id.clone(), session)]),
                order: vec![id],
                streams: BTreeMap::new(),
                running: None,
                revision: 0,
                credentials,
            })),
            events,
            client,
            stopping: CancellationToken::new(),
        })
    }
    pub async fn shutdown(&self) {
        self.stopping.cancel();
        let cancel = self
            .core
            .lock()
            .await
            .running
            .as_ref()
            .map(|r| r.cancel.clone());
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        loop {
            if self.core.lock().await.running.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}
// Mutations require a header that cross-origin HTML forms cannot send. No CORS grant is issued.
async fn local_only(request: Request, next: Next) -> Response {
    let local = |value: &str| {
        reqwest::Url::parse(value)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
    };
    let headers = request.headers();
    let valid_host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| local(&format!("http://{host}")));
    let valid_origin = headers
        .get("origin")
        .is_none_or(|v| v.to_str().is_ok_and(local));
    let mutation = !matches!(*request.method(), Method::GET | Method::HEAD);
    let client = headers.get("x-mnemoarc-client").is_some_and(|v| v == "web");
    if !valid_host || !valid_origin || (mutation && !client) {
        return ApiError(
            StatusCode::FORBIDDEN,
            "로컬 MnemoArc 화면에서 요청하세요.".into(),
        )
        .into_response();
    }
    next.run(request).await
}
pub fn router(state: WebState, frontend: PathBuf) -> Router {
    app_router(state, Some(frontend))
}

pub fn app_router(state: WebState, frontend: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route("/api/shutdown", post(shutdown_request))
        .route("/api/state", get(state_get))
        .route("/api/events", get(events))
        .route("/api/settings", put(settings))
        .route("/api/check", post(check))
        .route("/api/directories", get(directories))
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/{id}", get(session_get).delete(close_session))
        .route("/api/sessions/{id}/run", post(run))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/settings", put(session_settings))
        .route("/api/sessions/{id}/project", put(session_project))
        .route("/api/sessions/{id}/tools", put(session_tools))
        .route("/api/sessions/{id}/memories/{memory}", get(memory_get))
        .route("/api/sessions/{id}/output", get(output));
    let router = match frontend {
        Some(path) => {
            router.fallback_service(ServeDir::new(path).append_index_html_on_directories(true))
        }
        None => router.fallback(get(crate::desktop::embedded_ui)),
    };
    router
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(local_only))
        .with_state(state)
}
async fn shutdown_request(State(s): State<WebState>) -> Json<Value> {
    s.stopping.cancel();
    Json(json!({"stopping": true}))
}

async fn events(
    State(s): State<WebState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let receiver = s.events.subscribe();
    let stream = futures_util::stream::unfold(
        (receiver, true, s.stopping),
        |(mut receiver, first, stopping)| async move {
            if stopping.is_cancelled() {
                return None;
            }
            if !first {
                tokio::select! {
                    _ = stopping.cancelled() => return None,
                    result = receiver.recv() => if matches!(result, Err(broadcast::error::RecvError::Closed)) { return None; },
                }
            }
            Some((
                Ok(Event::default().event("changed").data("refresh")),
                (receiver, false, stopping),
            ))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
async fn state_get(State(s): State<WebState>) -> Json<Value> {
    let c = s.core.lock().await;
    Json(
        json!({"revision":c.revision,"config":c.config,"defaults":Config::default(),"credential":{"configured":OpenAiClient::has_key(&c.config),"saved":c.credentials.contains_key(&c.config.api_key_env)},"running":c.running.as_ref().map(|r|json!({"id":r.id,"closing":r.closing})),"sessions":c.order.iter().filter_map(|id|c.sessions.get(id)).map(|v|json!({"id":v.id,"project":v.project,"status":v.status,"title":v.latest_request.chars().take(60).collect::<String>(),"memory_count":v.memory.entries.len()})).collect::<Vec<_>>(),"tools":ToolRegistry::specs().iter().map(|t|json!({"name":t.name,"description":t.description,"optional":t.optional})).collect::<Vec<_>>()}),
    )
}
#[derive(Default, Deserialize)]
struct Page {
    before: Option<u64>,
    limit: Option<usize>,
}
async fn session_get(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Api {
    let mut c = s.core.lock().await;
    let state_changed = {
        let session = c.sessions.get_mut(&id).ok_or_else(missing)?;
        let generation = session.memory.generation;
        let investigations: Vec<_> = session
            .investigations
            .iter()
            .map(|item| (item.id.clone(), item.status.clone(), item.note.clone()))
            .collect();
        tools::revalidate(session)?;
        generation != session.memory.generation
            || investigations
                != session
                    .investigations
                    .iter()
                    .map(|item| (item.id.clone(), item.status.clone(), item.note.clone()))
                    .collect::<Vec<_>>()
    };
    if state_changed {
        changed(&s, &mut c);
    }
    let session = c.sessions.get(&id).ok_or_else(missing)?;
    let mut bundles = session
        .history
        .bundles
        .iter()
        .rev()
        .filter(|b| page.before.is_none_or(|before| b.id < before))
        .take(page.limit.unwrap_or(50).clamp(1, 100))
        .collect::<Vec<_>>();
    bundles.reverse();
    let previous = bundles
        .first()
        .filter(|b| session.history.bundles.iter().any(|old| old.id < b.id))
        .map(|b| b.id);
    Ok(Json(
        json!({"revision":c.revision,"id":id,"project":session.project,"config":session.config,"pending_config":session.pending_config,"credential_configured":OpenAiClient::has_key(&session.config),"status":session.status,"error":session.last_error,"task":session.task,"document_review":session.document_review,"run_guidance":session.run_guidance,"activity":session.activity,"continuation_pending":session.continuation.is_some(),"bundles":bundles,"previous":previous,"pruned_through":session.history.pruned_through,"stream":c.streams.get(&id),"memories":session.memory.recent(session.config.memory_count),"investigations":session.investigations,"active_tools":session.active_tools,"usage":{"input":session.input_tokens,"output":session.output_tokens,"cached":session.cached_tokens,"estimated":session.usage_incomplete,"context_estimated":crate::context::is_estimated(&session.config.model),"memory_bytes":session.memory.bytes(),"history_bytes":session.history.bytes(),"checkpoints":session.checkpoints_completed},"checkpoint":session.checkpoint}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    config: Config,
    api_key: Option<String>,
    #[serde(default = "keep")]
    credential_mode: String,
}
fn keep() -> String {
    "keep".into()
}
fn prepare_settings(
    input: &Settings,
    old: &Config,
    credentials: &BTreeMap<String, Secret>,
) -> Result<Config> {
    let mut config = input.config.clone();
    config.validate()?;
    for p in &mut config.projects {
        *p = normalize_project(p.clone())?;
    }
    config.api_key = if config.api_key_env == old.api_key_env {
        old.api_key.clone()
    } else {
        credentials.get(&config.api_key_env).cloned()
    };
    match input.credential_mode.as_str() {
        "keep" => {}
        "session" | "save" => {
            let value = input
                .api_key
                .as_ref()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("API 키를 입력하세요."))?;
            config.api_key = Some(Secret(value.clone()));
        }
        "clear" => {
            config.api_key = None;
        }
        _ => bail!("잘못된 인증 저장 방식입니다."),
    }
    Ok(config)
}
async fn settings(State(s): State<WebState>, Json(input): Json<Settings>) -> Api {
    let mut c = s.core.lock().await;
    let config = prepare_settings(&input, &c.config, &c.credentials)?;
    let mut credentials = c.credentials.clone();
    if input.credential_mode == "save" {
        credentials.insert(config.api_key_env.clone(), config.api_key.clone().unwrap());
    }
    if input.credential_mode == "clear" {
        credentials.remove(&config.api_key_env);
    }
    // Validate everything before writing. Keys are stored separately and never included in API responses.
    if input.credential_mode == "save" || input.credential_mode == "clear" {
        write_credentials(&credential_path(&c.path), &credentials)?;
    }
    config.save(&c.path)?;
    c.config = config;
    c.credentials = credentials;
    changed(&s, &mut c);
    Ok(Json(json!({"saved":true})))
}
async fn session_settings(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Settings>,
) -> Api {
    if input.credential_mode == "save" {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "키를 저장하려면 전체 설정을 사용하세요.".into(),
        ));
    }
    let mut c = s.core.lock().await;
    let old = c.sessions.get(&id).ok_or_else(missing)?;
    let config = prepare_settings(&input, &old.config, &c.credentials)?;
    let pending = if let Some(r) = &c.running
        && r.id == id
    {
        r.commands
            .try_send(RunCommand::Configure(Box::new(config.clone())))
            .map_err(|_| busy())?;
        c.sessions.get_mut(&id).unwrap().pending_config = Some(config);
        true
    } else {
        let session = c.sessions.get_mut(&id).unwrap();
        match agent::apply_config(session, config.clone()) {
            Ok(()) => {
                session.pending_config = None;
                false
            }
            Err(_) => {
                session.pending_config = Some(config);
                true
            }
        }
    };
    changed(&s, &mut c);
    Ok(Json(json!({"saved":true,"pending":pending})))
}
async fn check(State(s): State<WebState>, Json(input): Json<Settings>) -> Api {
    let config = {
        let c = s.core.lock().await;
        prepare_settings(&input, &c.config, &c.credentials)?
    };
    let message = tokio::select! {
        biased;
        _ = s.stopping.cancelled() => return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "앱을 종료하고 있습니다.".into())),
        result = OpenAiClient.probe(&config) => result?,
    };
    Ok(Json(json!({"message":message})))
}
#[derive(Deserialize)]
struct NewSession {
    project: Project,
}
async fn create_session(State(s): State<WebState>, Json(input): Json<NewSession>) -> Api {
    let project = normalize_project(input.project)?;
    let mut c = s.core.lock().await;
    let session = Session::new(project, c.config.clone());
    let id = session.id.clone();
    c.order.push(id.clone());
    c.sessions.insert(id.clone(), session);
    changed(&s, &mut c);
    Ok(Json(json!({"id":id})))
}
async fn session_project(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(project): Json<Project>,
) -> Api {
    let project = normalize_project(project)?;
    let mut c = s.core.lock().await;
    if c.running.as_ref().is_some_and(|r| r.id == id) {
        return Err(busy());
    }
    let session = c.sessions.get_mut(&id).ok_or_else(missing)?;
    let output_changed = tools::output_path(&project)? != tools::output_path(&session.project)?;
    if (project.root != session.project.root || output_changed)
        && !session.history.bundles.is_empty()
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "기록이 있는 세션의 소스 폴더나 결과 문서 경로는 변경할 수 없습니다. 새 세션을 만드세요.".into(),
        ));
    }
    session.project = project;
    tools::revalidate(session)?;
    changed(&s, &mut c);
    Ok(Json(json!({"saved":true})))
}
#[derive(Deserialize)]
struct ToolSelection {
    names: BTreeSet<String>,
}
async fn session_tools(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(input): Json<ToolSelection>,
) -> Api {
    let optional: BTreeSet<_> = ToolRegistry::specs()
        .iter()
        .filter(|t| t.optional)
        .map(|t| t.name.to_string())
        .collect();
    if !input.names.is_subset(&optional) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "선택할 수 없는 도구입니다.".into(),
        ));
    }
    let mut c = s.core.lock().await;
    if !c.sessions.contains_key(&id) {
        return Err(missing());
    }
    if let Some(r) = &c.running
        && r.id == id
    {
        r.commands
            .try_send(RunCommand::Tools(input.names))
            .map_err(|_| busy())?;
    } else {
        c.sessions.get_mut(&id).unwrap().active_tools = input.names;
    }
    changed(&s, &mut c);
    Ok(Json(json!({"saved":true})))
}
#[derive(Deserialize)]
struct RunInput {
    #[serde(default)]
    text: String,
    #[serde(default = "chat")]
    action: String,
}
fn chat() -> String {
    "chat".into()
}
async fn run(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(input): Json<RunInput>,
) -> Api {
    if s.stopping.is_cancelled() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "앱을 종료하고 있습니다.".into(),
        ));
    }
    let (session, cancel, rx) = {
        let mut c = s.core.lock().await;
        // Admission and shutdown must agree under the same lock.
        if s.stopping.is_cancelled() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "앱을 종료하고 있습니다.".into(),
            ));
        }
        if c.running.is_some() {
            return Err(busy());
        }
        let session = c.sessions.get_mut(&id).ok_or_else(missing)?;
        session.config.runnable()?;
        match input.action.as_str() {
            "chat" => { if input.text.trim().is_empty(){return Err(ApiError(StatusCode::BAD_REQUEST,"메시지를 입력하세요.".into()));} session.add_user(input.text); },
            "resume" => {},
            "cleanup" => session.add_user("Clean up memory and progress to fit the pending settings. Preserve important evidence and user constraints. Do not modify project files.".into()),
            _ => return Err(ApiError(StatusCode::BAD_REQUEST,"지원하지 않는 실행 방식입니다.".into())),
        }
        if let Some(cp) = &mut session.checkpoint {
            cp.attempts = 0;
            cp.acknowledged = false;
            cp.failed_attempts = 0;
            cp.last_failure = None;
            cp.failed = false;
        }
        session.status = "running".into();
        session.activity =
            json!({"stage":"preparing","started_at_ms":chrono::Utc::now().timestamp_millis()});
        session.last_error = None;
        let copy = session.clone();
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(16);
        c.running = Some(Running {
            id: id.clone(),
            cancel: cancel.clone(),
            commands: tx,
            closing: false,
        });
        changed(&s, &mut c);
        (copy, cancel, rx)
    };
    let state = s.clone();
    tokio::spawn(async move {
        let (tx, mut events) = mpsc::channel(128);
        let owner = state.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let mut c = owner.core.lock().await;
                match event {
                    AgentEvent::Delta { session, text } => {
                        let closing = c
                            .running
                            .as_ref()
                            .is_some_and(|running| running.id == session && running.closing);
                        if !closing {
                            c.streams.entry(session).or_default().push_str(&text);
                        }
                    }
                    AgentEvent::Snapshot(snapshot) => {
                        let mut snapshot = *snapshot;
                        let closing = c
                            .running
                            .as_ref()
                            .is_some_and(|running| running.id == snapshot.id && running.closing);
                        if closing {
                            c.streams.remove(&snapshot.id);
                            continue;
                        }
                        // A source can change while the agent is running. The
                        // snapshot was produced from the agent's private copy,
                        // so revalidate it before publishing it or it could
                        // overwrite a fresher session_get result.
                        let _ = tools::revalidate(&mut snapshot);
                        let advanced = c
                            .sessions
                            .get(&snapshot.id)
                            .is_some_and(|old| old.history.next_id != snapshot.history.next_id);
                        if advanced {
                            c.streams.remove(&snapshot.id);
                        }
                        c.sessions.insert(snapshot.id.clone(), snapshot);
                    }
                    AgentEvent::Tool { session, .. } => {
                        c.streams.remove(&session);
                    }
                    AgentEvent::Notice { session, text } => {
                        let closing = c
                            .running
                            .as_ref()
                            .is_some_and(|running| running.id == session && running.closing);
                        if !closing
                            && let Some(v) = c.sessions.get_mut(&session)
                        {
                            v.last_error = Some(text);
                        }
                    }
                }
                changed(&owner, &mut c);
            }
        });
        let job = agent::run_session_controlled(session, state.client.clone(), cancel, tx, rx);
        let outcome = std::panic::AssertUnwindSafe(job).catch_unwind().await;
        let _ = pump.await;
        let mut c = state.core.lock().await;
        c.streams.remove(&id);
        if c.running.as_ref().is_some_and(|r| r.closing) {
            c.sessions.remove(&id);
            c.order.retain(|old| old != &id);
        } else {
            match outcome {
                Ok(session) => {
                    let mut session = session;
                    let _ = tools::revalidate(&mut session);
                    c.sessions.insert(id.clone(), session);
                }
                Err(_) => {
                    if let Some(session) = c.sessions.get_mut(&id) {
                        session.status = "blocked".into();
                        session.activity = json!({"stage":"idle"});
                        session.last_error = Some("agent_worker_panic: last snapshot retained; write outcomes require review".into());
                    }
                }
            }
        }
        c.running = None;
        changed(&state, &mut c);
    });
    Ok(Json(json!({"started":true})))
}
async fn cancel(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let c = s.core.lock().await;
    if !c.sessions.contains_key(&id) {
        return Err(missing());
    }
    if let Some(r) = &c.running
        && r.id == id
    {
        r.cancel.cancel();
    }
    Ok(Json(json!({"cancelled":true})))
}
async fn close_session(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let mut c = s.core.lock().await;
    if !c.sessions.contains_key(&id) {
        return Err(missing());
    }
    if let Some(r) = &mut c.running
        && r.id == id
    {
        r.closing = true;
        r.cancel.cancel();
        c.streams.remove(&id);
    } else {
        c.sessions.remove(&id);
        c.streams.remove(&id);
        c.order.retain(|old| old != &id);
    }
    changed(&s, &mut c);
    Ok(Json(json!({"closed":true})))
}
async fn memory_get(State(s): State<WebState>, Path((id, memory)): Path<(String, String)>) -> Api {
    let mut c = s.core.lock().await;
    let session = c.sessions.get_mut(&id).ok_or_else(missing)?;
    let generation = session.memory.generation;
    tools::revalidate(session)?;
    let value = session.memory.get(&memory).cloned();
    if session.memory.generation != generation {
        changed(&s, &mut c);
    }
    Ok(Json(json!(value?)))
}
async fn output(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let project = {
        let c = s.core.lock().await;
        c.sessions.get(&id).ok_or_else(missing)?.project.clone()
    };
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let path = tools::output_path(&project)?;
        let (content, truncated) = tools::read_text_preview(&path, 2 * 1024 * 1024)?;
        Ok(json!({"path":path,"content":content,"truncated":truncated}))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}
#[derive(Deserialize)]
struct DirectoryQuery {
    path: Option<PathBuf>,
}
async fn directories(Query(query): Query<DirectoryQuery>) -> Api {
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let path = query
            .path
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?;
        let mut entries = std::fs::read_dir(&path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| json!({"name":e.file_name().to_string_lossy(),"path":e.path()}))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"path":path,"parent":path.parent(),"directories":entries}))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(result))
}
pub async fn serve(config: Config, path: PathBuf, port: u16, frontend: PathBuf) -> Result<()> {
    serve_managed(config, path, port, frontend, false).await
}
pub async fn serve_managed(
    config: Config,
    path: PathBuf,
    port: u16,
    frontend: PathBuf,
    shutdown_on_stdin: bool,
) -> Result<()> {
    serve_app(
        config,
        path,
        Some(port),
        Some(frontend),
        shutdown_on_stdin,
        false,
    )
    .await
}

pub async fn serve_app(
    config: Config,
    path: PathBuf,
    port: Option<u16>,
    frontend: Option<PathBuf>,
    shutdown_on_stdin: bool,
    open_browser: bool,
) -> Result<()> {
    if let Some(frontend) = &frontend {
        anyhow::ensure!(
            frontend.join("index.html").is_file(),
            "UI index.html missing in {}",
            frontend.display()
        );
    }
    let state = WebState::new(config, path, Arc::new(OpenAiClient))?;
    let listener =
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port.unwrap_or(3030)))
            .await
        {
            Ok(listener) => listener,
            Err(error) if port.is_none() && error.kind() == std::io::ErrorKind::AddrInUse => {
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?
            }
            Err(error) => return Err(error.into()),
        };
    println!(
        "MnemoArc: http://127.0.0.1:{}",
        listener.local_addr()?.port()
    );
    let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    // The listener is bound before launching the browser; failures leave a usable printed URL.
    if open_browser {
        std::thread::spawn(move || {
            if let Err(error) = crate::desktop::open_browser(&url) {
                eprintln!("브라우저를 열지 못했습니다: {error}. 직접 접속하세요: {url}");
            }
        });
    }
    let (stdin_closed, closed) = tokio::sync::oneshot::channel();
    if shutdown_on_stdin {
        // A plain thread avoids Tokio's non-cancellable stdin reader preventing
        // runtime shutdown after Ctrl+C. EOF also works for Windows launchers.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            let _ = stdin_closed.send(());
        });
    }
    let shutdown = state.clone();
    axum::serve(listener, app_router(state, frontend))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = shutdown.stopping.cancelled() => {},
                _ = closed, if shutdown_on_stdin => {},
            }
            shutdown.shutdown().await;
        })
        .await?;
    Ok(())
}
