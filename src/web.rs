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
    body::{Body, Bytes, HttpBody},
    extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post, put},
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

mod connections;
mod file_checks;
mod file_io;
mod session_view;
use connections::ManagedListener;
use file_checks::FileChecks;
use file_io::FileIo;

struct Running {
    cancel: CancellationToken,
    commands: mpsc::Sender<RunCommand>,
    closing: bool,
}
struct Core {
    config: Config,
    path: PathBuf,
    sessions: BTreeMap<String, Arc<session_view::Snapshot>>,
    order: Vec<String>,
    streams: BTreeMap<String, String>,
    running: BTreeMap<String, Running>,
    revision: u64,
    session_revisions: BTreeMap<String, u64>,
    credentials: BTreeMap<String, Secret>,
}
impl Core {
    fn session_mut(&mut self, id: &str) -> Option<&mut Session> {
        Some(Arc::make_mut(self.sessions.get_mut(id)?).session_mut())
    }

    fn remove_session(&mut self, id: &str) {
        self.sessions.remove(id);
        self.session_revisions.remove(id);
        self.streams.remove(id);
        self.order.retain(|old| old != id);
        // Retaining fewer IDs must also release storage after a large batch
        // of session closes, including the last running session's settlement.
        if self.order.capacity() > self.order.len().saturating_mul(2) {
            self.order.shrink_to_fit();
        }
    }
}
#[derive(Clone)]
pub struct WebState {
    core: Arc<Mutex<Core>>,
    settings_gate: Arc<Mutex<()>>,
    events: broadcast::Sender<Change>,
    client: Arc<dyn LlmClient>,
    stopping: CancellationToken,
    file_io: FileIo,
    file_checks: FileChecks,
    write_outcome_uncertain: Arc<AtomicBool>,
}
#[derive(Debug)]
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
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
#[derive(Clone, Debug, Default, Serialize)]
struct Change {
    revision: u64,
    session: Option<String>,
    state: bool,
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "세션을 찾을 수 없습니다.".into())
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "이 세션의 작업이 실행 중입니다. 완료되거나 중지된 뒤 다시 시도하세요.".into(),
    )
}
fn changed(s: &WebState, c: &mut Core) {
    c.revision = c.revision.saturating_add(1);
    for id in c.sessions.keys() {
        c.session_revisions.insert(id.clone(), c.revision);
    }
    let _ = s.events.send(Change {
        revision: c.revision,
        session: None,
        state: true,
    });
}
fn session_changed(s: &WebState, c: &mut Core, id: &str, state: bool) {
    c.revision = c.revision.saturating_add(1);
    if c.sessions.contains_key(id) {
        c.session_revisions.insert(id.to_owned(), c.revision);
    }
    let _ = s.events.send(Change {
        revision: c.revision,
        session: Some(id.into()),
        state,
    });
}
fn session_summary(session: &Session) -> Value {
    json!({"id":session.id,"project":session.project,"status":session.status,"title":session.latest_request.chars().take(60).collect::<String>(),"memory_count":session.memory.entries.len()})
}
fn same_config(a: &Config, b: &Config) -> bool {
    // Config serialization intentionally omits the secret, so compare it
    // separately when deciding whether an in-flight setting command applied.
    let (Ok(serialized_a), Ok(serialized_b)) = (serde_json::to_value(a), serde_json::to_value(b))
    else {
        return false;
    };
    a.api_key.as_ref().map(|key| &key.0) == b.api_key.as_ref().map(|key| &key.0)
        && serialized_a == serialized_b
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
fn restore_file(path: &FsPath, original: Option<&(Vec<u8>, std::fs::Permissions)>) -> Result<()> {
    match original {
        Some((bytes, permissions)) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(FsPath::new("."));
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            use std::io::Write;
            file.write_all(bytes)?;
            file.as_file().set_permissions(permissions.clone())?;
            file.as_file().sync_all()?;
            file.persist(path)?;
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(anyhow::Error::from(error)),
        },
    }
    Ok(())
}
fn normalize_project(mut project: Project) -> Result<Project> {
    if project.root.as_os_str().is_empty() {
        bail!("프로젝트 소스 폴더가 필요합니다.");
    }
    if project.output.as_os_str().is_empty() {
        bail!("프로젝트 결과 문서 경로가 필요합니다.");
    }
    project.root = project.root.canonicalize()?;
    project.validate_paths()?;
    if !project.root.is_dir() || project.name.trim().is_empty() {
        bail!("프로젝트 이름과 실제 폴더가 필요합니다.");
    }
    tools::output_path(&project)?;
    for pattern in project.include.iter().chain(&project.exclude) {
        globset::Glob::new(pattern)?;
    }
    Ok(project)
}
// Older clients omit IDs. Reuse a saved identity only for one exact definition;
// matching by folder (or name) would merge distinct projects again.
fn inherit_project_id(project: &mut Project, saved: &[Project]) {
    if !project.id.trim().is_empty() {
        return;
    }
    let mut matches = saved.iter().filter(|candidate| {
        let mut definition = project.clone();
        definition.id.clone_from(&candidate.id);
        &definition == *candidate
    });
    if let Some(candidate) = matches.next()
        && matches.next().is_none()
    {
        project.id.clone_from(&candidate.id);
    }
}
impl WebState {
    pub fn new(mut config: Config, path: PathBuf, client: Arc<dyn LlmClient>) -> Result<Self> {
        let credentials: BTreeMap<String, Secret> = if credential_path(&path).exists() {
            let data: BTreeMap<String, String> =
                serde_json::from_str(&tools::read_text(&credential_path(&path))?)?;
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
        config.ensure_project_ids()?;
        config.validate()?;
        // Saved folders may have been moved or temporarily unmounted. Keep
        // those entries editable in settings, and start with an available
        // project instead of preventing the settings UI from opening.
        let mut initial_project = None;
        for project in &mut config.projects {
            if let Ok(normalized) = normalize_project(project.clone()) {
                *project = normalized;
                initial_project.get_or_insert_with(|| project.clone());
            }
        }
        let write_outcome_uncertain = Arc::new(AtomicBool::new(false));
        let mut sessions = BTreeMap::new();
        let mut order = Vec::new();
        if let Some(project) = initial_project {
            let mut session = Session::new(project, config.clone());
            session.write_outcome_uncertain = write_outcome_uncertain.clone();
            order.push(session.id.clone());
            sessions.insert(
                session.id.clone(),
                Arc::new(session_view::Snapshot::new(session)),
            );
        }
        let (events, _) = broadcast::channel(128);
        let stopping = CancellationToken::new();
        Ok(Self {
            core: Arc::new(Mutex::new(Core {
                config,
                path,
                sessions,
                order,
                streams: BTreeMap::new(),
                running: BTreeMap::new(),
                revision: 0,
                session_revisions: BTreeMap::new(),
                credentials,
            })),
            settings_gate: Arc::new(Mutex::new(())),
            events,
            client,
            file_io: FileIo::new(stopping.clone()),
            file_checks: FileChecks::default(),
            stopping,
            write_outcome_uncertain,
        })
    }
    pub async fn shutdown(&self) {
        self.stopping.cancel();
        let cancellations: Vec<_> = self
            .core
            .lock()
            .await
            .running
            .values()
            .map(|r| r.cancel.clone())
            .collect();
        for cancel in cancellations {
            cancel.cancel();
        }
        loop {
            if self.core.lock().await.running.is_empty() {
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
    if request.body().is_end_stream() {
        return next.run(request).await;
    }
    // Size limits alone do not release a peer that declares a body and never
    // finishes it. Bound body receipt before handing it to any API extractor;
    // model checks and response/SSE streams retain their own longer deadlines.
    let (parts, body) = request.into_parts();
    let buffered = tokio::time::timeout(
        REQUEST_BODY_TIMEOUT,
        Bytes::from_request(Request::from_parts(parts.clone(), body), &()),
    )
    .await;
    let bytes = match buffered {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            return ([(header::CONNECTION, "close")], error.into_response()).into_response();
        }
        Err(_) => {
            return (
                [(header::CONNECTION, "close")],
                ApiError(
                    StatusCode::REQUEST_TIMEOUT,
                    "요청 데이터를 받는 시간이 초과되었습니다. 다시 시도하세요.".into(),
                ),
            )
                .into_response();
        }
    };
    let request = Request::from_parts(parts, Body::from(bytes));
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
        .route("/api/sessions/{id}/check", post(session_check))
        .route("/api/sessions/{id}/project", put(session_project))
        .route("/api/sessions/{id}/tools", put(session_tools))
        .route("/api/sessions/{id}/workflow", put(session_workflow))
        .route("/api/sessions/{id}/memories/{memory}", get(memory_get))
        .route("/api/sessions/{id}/output", get(output))
        .route("/api/sessions/{id}/output/download", get(output_download));
    let router = match frontend {
        Some(path) => {
            router.fallback_service(ServeDir::new(path).append_index_html_on_directories(true))
        }
        None => router.fallback(get(crate::desktop::embedded_ui)),
    };
    router
        .layer(middleware::from_fn(local_only))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
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
            let change = if !first {
                tokio::select! {
                    _ = stopping.cancelled() => return None,
                    result = receiver.recv() => match result {
                        Ok(change) => change,
                        Err(broadcast::error::RecvError::Closed) => return None,
                        // A slow client may miss multiple sessions' events.
                        Err(broadcast::error::RecvError::Lagged(_)) => Change { state: true, ..Default::default() },
                    },
                }
            } else {
                Change {
                    state: true,
                    ..Default::default()
                }
            };
            Some((
                Ok(Event::default()
                    .event("changed")
                    .json_data(change)
                    .expect("change is JSON serializable")),
                (receiver, false, stopping),
            ))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
async fn state_get(State(s): State<WebState>) -> Json<Value> {
    let c = s.core.lock().await;
    Json(
        json!({"revision":c.revision,"config":c.config,"defaults":Config::default(),"credential":{"configured":OpenAiClient::has_key(&c.config),"saved":c.credentials.contains_key(&c.config.api_key_env)},"running":c.order.iter().filter_map(|id| c.running.get(id).map(|r| json!({"id":id,"closing":r.closing}))).collect::<Vec<_>>(),"sessions":c.order.iter().filter_map(|id|c.sessions.get(id)).map(|v| { let mut summary = session_summary(v); summary["revision"] = json!(c.session_revisions.get(&v.id).copied().unwrap_or(0)); summary }).collect::<Vec<_>>(),"tools":ToolRegistry::specs().iter().filter(|t|t.name != "db_query" && t.name != "db_execute").map(|t|json!({"name":t.name,"description":t.description,"optional":t.optional})).collect::<Vec<_>>()}),
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
    session_view::read(s, id, session_view::Read::Page(page)).await
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
    for p in &mut config.projects {
        *p = normalize_project(p.clone())?;
        inherit_project_id(p, &old.projects);
    }
    config.ensure_project_ids()?;
    config.validate()?;
    if config.projects.is_empty() {
        bail!("At least one project is required");
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
    let (gate, deadline) = acquire_settings_gate(&s).await?;
    let state = s.clone();
    s.file_io
        .run_queued(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            move |cancel| {
                // Keep serialization with the actual worker: a timed-out or dropped
                // HTTP request must not let a later save overtake unfinished file I/O.
                let _gate = gate;
                save_settings(&state, input, &cancel)
            },
        )
        .await?;
    Ok(Json(json!({"saved":true})))
}

async fn acquire_settings_gate(
    s: &WebState,
) -> Result<(tokio::sync::OwnedMutexGuard<()>, tokio::time::Instant), ApiError> {
    let timeout = Duration::from_secs(s.core.lock().await.config.tool_timeout_secs);
    let deadline = tokio::time::Instant::now() + timeout;
    let gate = tokio::select! {
        biased;
        _ = s.stopping.cancelled() => return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "앱을 종료하고 있습니다.".into())),
        result = tokio::time::timeout_at(deadline, s.settings_gate.clone().lock_owned()) => result.map_err(|_| ApiError(StatusCode::GATEWAY_TIMEOUT, "file_operation_timeout: 설정 저장 제한 시간을 초과했습니다.".into()))?,
    };
    Ok((gate, deadline))
}

fn save_settings(s: &WebState, input: Settings, cancel: &CancellationToken) -> Result<()> {
    let (old, path, mut credentials) = {
        let c = s.core.blocking_lock();
        (c.config.clone(), c.path.clone(), c.credentials.clone())
    };
    let config = prepare_settings(&input, &old, &credentials)?;
    let updates_credentials = matches!(input.credential_mode.as_str(), "save" | "clear");
    if input.credential_mode == "save" {
        credentials.insert(config.api_key_env.clone(), config.api_key.clone().unwrap());
    }
    if input.credential_mode == "clear" {
        credentials.remove(&config.api_key_env);
    }
    // Stage the old config before changing either file. Keys are stored
    // separately and never included in API responses. If credential
    // persistence fails after the config write, restore the exact previous
    // config so a failed request cannot leave only half of the settings live.
    let previous_config = if updates_credentials {
        match std::fs::metadata(&path) {
            Ok(_) => {
                use std::io::Read;
                let file = tools::open_regular_file(&path)?;
                let permissions = file.metadata()?.permissions();
                let mut bytes = Vec::new();
                file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 16 * 1024 * 1024 {
                    bail!("unsupported_large_file: configuration exceeds 16MiB");
                }
                Some((bytes, permissions))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    config.save(&path)?;
    if updates_credentials
        && let Err(error) = write_credentials(&credential_path(&path), &credentials)
    {
        if let Err(rollback) = restore_file(&path, previous_config.as_ref()) {
            return Err(anyhow::anyhow!(
                "credential save failed: {error}; config rollback failed: {rollback}"
            ));
        }
        return Err(error);
    }
    // Once persistence starts, publish the completed transaction even if the
    // requester disconnects, keeping live settings consistent with the files.
    let mut c = s.core.blocking_lock();
    c.config = config;
    c.credentials = credentials;
    changed(s, &mut c);
    Ok(())
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
    let (_gate, deadline) = acquire_settings_gate(&s).await?;
    let (current, credentials) = {
        let c = s.core.lock().await;
        let old = c.sessions.get(&id).ok_or_else(missing)?;
        // Secrets are omitted from API responses. A later `keep` request must
        // inherit the latest queued choice, including a queued key deletion.
        (
            old.pending_config.as_ref().unwrap_or(&old.config).clone(),
            c.credentials.clone(),
        )
    };
    let baseline = current.clone();
    let mut config = s
        .file_io
        .run_queued(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            move |_| prepare_settings(&input, &baseline, &credentials),
        )
        .await?;
    let mut c = s.core.lock().await;
    let old = c.sessions.get(&id).ok_or_else(missing)?;
    if !same_config(old.pending_config.as_ref().unwrap_or(&old.config), &current) {
        return Err(busy());
    }
    config.max_concurrent_sessions = c.config.max_concurrent_sessions;
    let pending = if let Some(r) = c.running.get(&id) {
        r.commands
            .try_send(RunCommand::Configure(Box::new(config.clone())))
            .map_err(|_| busy())?;
        c.session_mut(&id).unwrap().pending_config = Some(config);
        true
    } else {
        let session = c.session_mut(&id).unwrap();
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
    session_changed(&s, &mut c, &id, false);
    Ok(Json(json!({"saved":true,"pending":pending})))
}
async fn check(State(s): State<WebState>, Json(input): Json<Settings>) -> Api {
    let (old, credentials, timeout) = {
        let c = s.core.lock().await;
        (
            c.config.clone(),
            c.credentials.clone(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let config = s
        .file_io
        .run_queued(timeout, move |_| {
            prepare_settings(&input, &old, &credentials)
        })
        .await?;
    check_config(s, config).await
}
async fn session_check(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Settings>,
) -> Api {
    let (old, credentials, timeout) = {
        let c = s.core.lock().await;
        let session = c.sessions.get(&id).ok_or_else(missing)?;
        let current = session.pending_config.as_ref().unwrap_or(&session.config);
        (
            current.clone(),
            c.credentials.clone(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let config = s
        .file_io
        .run_queued(timeout, move |_| {
            prepare_settings(&input, &old, &credentials)
        })
        .await?;
    check_config(s, config).await
}
async fn check_config(s: WebState, config: Config) -> Api {
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
    let timeout = Duration::from_secs(s.core.lock().await.config.tool_timeout_secs);
    let mut project = s
        .file_io
        .run_queued(timeout, move |_| normalize_project(input.project))
        .await?;
    let mut c = s.core.lock().await;
    inherit_project_id(&mut project, &c.config.projects);
    let mut session = Session::new(project, c.config.clone());
    session.write_outcome_uncertain = s.write_outcome_uncertain.clone();
    let id = session.id.clone();
    c.order.push(id.clone());
    c.sessions
        .insert(id.clone(), Arc::new(session_view::Snapshot::new(session)));
    session_changed(&s, &mut c, &id, true);
    Ok(Json(json!({"id":id})))
}
async fn session_project(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(project): Json<Project>,
) -> Api {
    let (original, timeout) = {
        let c = s.core.lock().await;
        if c.running.contains_key(&id) {
            return Err(busy());
        }
        (
            c.sessions.get(&id).ok_or_else(missing)?.clone(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let baseline = original.clone();
    let session = s
        .file_io
        .run_queued(timeout, move |cancel| {
            Ok(prepare_project_update(&baseline, project, &cancel))
        })
        .await??;
    let mut c = s.core.lock().await;
    if c.running.contains_key(&id) {
        return Err(busy());
    }
    let current = c.sessions.get(&id).ok_or_else(missing)?;
    if !Arc::ptr_eq(current, &original) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "세션 상태가 변경되었습니다. 최신 상태에서 다시 시도하세요.".into(),
        ));
    }
    c.sessions
        .insert(id.clone(), Arc::new(session_view::Snapshot::new(session)));
    session_changed(&s, &mut c, &id, true);
    Ok(Json(json!({"saved":true})))
}

fn prepare_project_update(
    original: &session_view::Snapshot,
    project: Project,
    cancel: &CancellationToken,
) -> Result<Session, ApiError> {
    let mut project = normalize_project(project)?;
    let mut session = original.session.clone();
    if project.id.trim().is_empty() {
        project.id.clone_from(&session.project.id);
    }
    if project.id != session.project.id {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "세션의 프로젝트 소속은 변경할 수 없습니다. 다른 프로젝트에서 새 세션을 만드세요."
                .into(),
        ));
    }
    let output_changed = tools::output_path(&project)? != tools::output_path(&session.project)?;
    // Check the durable history counter. A checkpoint can prune every bundle
    // while the task, memories and source references still belong to the old
    // project; an empty deque does not make the session new again.
    if (project.root != session.project.root || output_changed) && session.history.next_id > 0 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "기록이 있는 세션의 소스 폴더나 결과 문서 경로는 변경할 수 없습니다. 새 세션을 만드세요.".into(),
        ));
    }
    session.project = project;
    tools::inspect_freshness(&session, cancel)?.apply(&mut session);
    if cancel.is_cancelled() {
        return Err(anyhow::anyhow!("cancelled").into());
    }
    Ok(session)
}
#[derive(Deserialize)]
struct WorkflowSelection {
    workflow: String,
}
/// The user's choice of how the session's requests are handled. It applies
/// from the next request; the running one keeps the workflow it started with.
async fn session_workflow(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(input): Json<WorkflowSelection>,
) -> Api {
    if !crate::session::WORKFLOW_MODES.contains(&input.workflow.as_str()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "선택할 수 없는 작업 방식입니다.".into(),
        ));
    }
    let mut c = s.core.lock().await;
    if c.running.contains_key(&id) {
        return Err(busy());
    }
    let session = c.session_mut(&id).ok_or_else(missing)?;
    session.workflow_mode = input.workflow;
    session.drop_forbidden_workflow_tools();
    session_changed(&s, &mut c, &id, false);
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
    let session = c.sessions.get(&id).ok_or_else(missing)?;
    ToolRegistry::validate_tool_selection(session, &input.names)?;
    if let Some(r) = c.running.get(&id) {
        r.commands
            .try_send(RunCommand::Tools(input.names.clone()))
            .map_err(|_| busy())?;
        // Keep the latest request in the owner session as well. The agent
        // owns a private clone while running, so cancellation can otherwise
        // let its older active_tools overwrite a queued selection.
        c.session_mut(&id).unwrap().pending_tools = Some(input.names);
    } else {
        let session = c.session_mut(&id).unwrap();
        session.active_tools = input.names;
        // An explicit user selection supersedes any older model tool_select
        // that was waiting for a request boundary.
        session.pending_tools = None;
    }
    session_changed(&s, &mut c, &id, false);
    Ok(Json(json!({"saved":true})))
}
#[derive(Deserialize)]
struct RunInput {
    #[serde(default)]
    text: String,
    #[serde(default = "auto_action")]
    action: String,
}
fn auto_action() -> String {
    "auto".into()
}

fn publish_snapshot(s: &WebState, c: &mut Core, mut snapshot: Arc<session_view::Snapshot>) {
    let closing = c
        .running
        .get(&snapshot.id)
        .is_none_or(|running| running.closing);
    if closing {
        c.streams.remove(&snapshot.id);
        return;
    }
    let advanced = c
        .sessions
        .get(&snapshot.id)
        .is_some_and(|old| old.history.next_id != snapshot.history.next_id);
    if advanced {
        c.streams.remove(&snapshot.id);
    }
    // Preserve settings/tool choices accepted after the private agent copy
    // was made, including choices made while its file worker was validating.
    if let Some(owner) = c.sessions.get(&snapshot.id) {
        let session = Arc::make_mut(&mut snapshot).session_mut();
        if let Some(pending) = owner.pending_config.clone() {
            session.pending_config = (!same_config(&session.config, &pending)).then_some(pending);
        }
        if let Some(pending) = owner.pending_tools.clone() {
            let pending = ToolRegistry::normalize_tool_selection(session, &pending);
            if session.active_tools == pending {
                session.pending_tools = None;
            } else {
                session.active_tools = pending.clone();
                session.pending_tools = Some(pending);
            }
        }
    }
    let id = snapshot.id.clone();
    let state_changed = c
        .sessions
        .get(&id)
        .is_none_or(|old| session_summary(old) != session_summary(&snapshot));
    c.sessions.insert(id.clone(), snapshot);
    session_changed(s, c, &id, state_changed);
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
        if !c.sessions.contains_key(&id) {
            return Err(missing());
        }
        if c.running.contains_key(&id) {
            return Err(busy());
        }
        if c.running.len() >= c.config.max_concurrent_sessions {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "동시에 실행할 수 있는 세션 수({}개)에 도달했습니다. 작업이 끝난 뒤 다시 보내거나 전체 설정에서 한도를 조절하세요.",
                    c.config.max_concurrent_sessions
                ),
            ));
        }
        if s.write_outcome_uncertain.load(Ordering::Acquire) {
            return Err(ApiError(StatusCode::CONFLICT, "도구 쓰기 결과를 확인할 수 없습니다. 파일 또는 데이터베이스 변경을 확인하고 앱을 다시 시작하세요.".into()));
        }
        let session = c.session_mut(&id).ok_or_else(missing)?;
        session.config.runnable()?;
        let action = if input.action == "auto" {
            if Session::is_continuation(&input.text) && !session.latest_request.is_empty() {
                "resume"
            } else if !session.latest_request.is_empty() {
                "question"
            } else {
                "chat"
            }
        } else {
            input.action.as_str()
        };
        match action {
            "chat" => {
                if input.text.trim().is_empty() {
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        "메시지를 입력하세요.".into(),
                    ));
                }
                session.history.check_append(
                    vec![json!({"role":"user","content":input.text})],
                    session.config.history_bytes,
                )?;
                // A new task clears some old state. Validate the resulting
                // metadata before publishing it, preserving the current task
                // if the request itself cannot fit.
                let mut next = session.clone();
                next.start_new_task(input.text);
                next.check_runtime_capacity()?;
                *session = next;
            }
            "question" => session.queue_question(input.text)?,
            "resume" | "cleanup" => {
                let accepted_text = input.text.is_empty()
                    || (action == "resume" && Session::is_continuation(&input.text));
                if !accepted_text {
                    return Err(ApiError(StatusCode::BAD_REQUEST,"재개와 기억 정리는 메시지를 받지 않습니다. 새 요청은 새 작업으로 보내세요.".into()));
                }
                if session.latest_request.is_empty() {
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        "재개하거나 정리할 작업이 없습니다.".into(),
                    ));
                }
                if action == "cleanup" {
                    let text = "Clean up memory and progress to fit the pending settings. Preserve important evidence and user constraints. Do not modify project files.";
                    // Cleanup can retire history above the soft limit, but
                    // repeated failed cleanups cannot keep adding instructions.
                    session.history.check_append(
                        vec![json!({"role":"user","content":text,"maintenance":true})],
                        session.config.history_bytes.saturating_mul(2),
                    )?;
                    session.add_maintenance(text.into());
                }
            }
            _ => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "지원하지 않는 실행 방식입니다.".into(),
                ));
            }
        }
        if session.question.is_none()
            && let Some(cp) = &mut session.checkpoint
        {
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
        session.begin_run();
        let copy = session.clone();
        let cancel = s.stopping.child_token();
        let (tx, rx) = mpsc::channel(16);
        c.running.insert(
            id.clone(),
            Running {
                cancel: cancel.clone(),
                commands: tx,
                closing: false,
            },
        );
        session_changed(&s, &mut c, &id, true);
        (copy, cancel, rx)
    };
    let state = s.clone();
    tokio::spawn(async move {
        // Each snapshot owns the retained session, including its history.
        // Keep backpressure close to the owner instead of queuing 128 copies.
        let (tx, mut events) = mpsc::channel(4);
        let owner = state.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let AgentEvent::Snapshot(snapshot) = event {
                    let snapshot = session_view::prepare(&owner, *snapshot).await;
                    let mut c = owner.core.lock().await;
                    publish_snapshot(&owner, &mut c, snapshot);
                    continue;
                }
                let session_id = match &event {
                    AgentEvent::Delta { session, .. }
                    | AgentEvent::Tool { session, .. }
                    | AgentEvent::Notice { session, .. } => session.clone(),
                    AgentEvent::Snapshot(_) => unreachable!(),
                };
                let mut c = owner.core.lock().await;
                match event {
                    AgentEvent::Delta { session, text } => {
                        let closing = c
                            .running
                            .get(&session)
                            .is_none_or(|running| running.closing);
                        if !closing {
                            c.streams.entry(session).or_default().push_str(&text);
                        }
                    }
                    AgentEvent::Snapshot(_) => unreachable!(),
                    AgentEvent::Tool { session, .. } => {
                        c.streams.remove(&session);
                    }
                    AgentEvent::Notice { session, text } => {
                        let closing = c
                            .running
                            .get(&session)
                            .is_none_or(|running| running.closing);
                        if !closing && let Some(v) = c.session_mut(&session) {
                            v.last_error = Some(text);
                        }
                    }
                }
                session_changed(&owner, &mut c, &session_id, false);
            }
        });
        let outcome = {
            // Completion, a deadline and a panic all end this run's file
            // validation too. The event pump still publishes queued snapshots.
            let _cancel_on_finish = cancel.clone().drop_guard();
            let job = agent::run_session_controlled(session, state.client.clone(), cancel, tx, rx);
            std::panic::AssertUnwindSafe(job).catch_unwind().await
        };
        let _ = pump.await;
        let settled = std::panic::AssertUnwindSafe(async {
            let outcome = match outcome {
                Ok(session) => Ok(session_view::prepare(&state, session).await),
                Err(error) => Err(error),
            };
            let mut c = state.core.lock().await;
            c.streams.remove(&id);
            if c.running.get(&id).is_some_and(|r| r.closing) {
                c.remove_session(&id);
            } else {
                match outcome {
                    Ok(mut snapshot) => {
                        let session = Arc::make_mut(&mut snapshot).session_mut();
                        // The web owner records a pending setting before sending
                        // the command to the agent. If cancellation wins before
                        // that command is consumed, the private agent copy still
                        // has the old config and would otherwise erase the owner's
                        // pending change when its final snapshot is stored.
                        if let Some(pending) = c
                            .sessions
                            .get(&id)
                            .and_then(|owner| owner.pending_config.clone())
                        {
                            if same_config(&session.config, &pending) {
                                // The command was applied even if its clearing
                                // snapshot was not delivered before cancellation.
                                session.pending_config = None;
                            } else {
                                // Preserve the newest requested config for the
                                // next run; an older private pending value must not
                                // hide it.
                                session.pending_config = Some(pending);
                            }
                        }
                        if let Some(pending) = c
                            .sessions
                            .get(&id)
                            .and_then(|owner| owner.pending_tools.clone())
                        {
                            // Apply the same request-boundary normalization used
                            // by the running agent. This closes the cancellation
                            // path where a stale private copy would otherwise
                            // restore a selection that violates the final task
                            // workflow.
                            let pending = ToolRegistry::normalize_tool_selection(session, &pending);
                            if session.active_tools == pending {
                                // The command was consumed by the agent. A model
                                // tool_select queued on the private copy must not
                                // override the user's explicit selection.
                                session.pending_tools = None;
                            } else {
                                // The command was still queued when the run
                                // ended; make the user's latest selection active
                                // for the next run and discard stale private work.
                                session.active_tools = pending;
                                session.pending_tools = None;
                            }
                        }
                        c.sessions.insert(id.clone(), snapshot);
                    }
                    Err(_) => block_after_panic(&mut c, &id),
                }
            }
            c.running.remove(&id);
            session_changed(&state, &mut c, &id, true);
        })
        .catch_unwind()
        .await;
        // Settlement runs outside the agent's unwind boundary. A panic there
        // must still release the run slot, or the session stays busy and
        // shutdown waits for it forever.
        if settled.is_err() {
            let mut c = state.core.lock().await;
            if c.running.contains_key(&id) {
                c.streams.remove(&id);
                if c.running.get(&id).is_some_and(|r| r.closing) {
                    c.remove_session(&id);
                } else {
                    block_after_panic(&mut c, &id);
                }
                c.running.remove(&id);
                session_changed(&state, &mut c, &id, true);
            }
        }
    });
    Ok(Json(json!({"started":true})))
}
fn block_after_panic(c: &mut Core, id: &str) {
    if let Some(session) = c.session_mut(id) {
        session.status = "blocked".into();
        session.last_error = Some(
            "agent_worker_panic: last snapshot retained; write outcomes require review".into(),
        );
        session.note_run_estimate();
        session.finish_maintenance();
        session.finish_run();
        session.activity = json!({"stage":"idle"});
        session.restore_after_question();
    }
}
async fn cancel(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let c = s.core.lock().await;
    if !c.sessions.contains_key(&id) {
        return Err(missing());
    }
    if let Some(r) = c.running.get(&id) {
        r.cancel.cancel();
    }
    Ok(Json(json!({"cancelled":true})))
}
async fn close_session(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let mut c = s.core.lock().await;
    if !c.sessions.contains_key(&id) {
        return Err(missing());
    }
    if let Some(r) = c.running.get_mut(&id) {
        r.closing = true;
        r.cancel.cancel();
        c.streams.remove(&id);
    } else {
        c.remove_session(&id);
    }
    session_changed(&s, &mut c, &id, true);
    Ok(Json(json!({"closed":true})))
}
async fn memory_get(State(s): State<WebState>, Path((id, memory)): Path<(String, String)>) -> Api {
    session_view::read(s, id, session_view::Read::Memory(memory)).await
}
async fn output(State(s): State<WebState>, Path(id): Path<String>) -> Api {
    let (project, timeout) = {
        let c = s.core.lock().await;
        (
            c.sessions.get(&id).ok_or_else(missing)?.project.clone(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let result = s
        .file_io
        .run(timeout, move |_| -> Result<Value> {
            let path = tools::output_path(&project)?;
            let (content, truncated) = tools::read_text_preview(&path, 2 * 1024 * 1024)?;
            Ok(json!({"path":path,"content":content,"truncated":truncated}))
        })
        .await?;
    Ok(Json(result))
}
async fn output_download(
    State(s): State<WebState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (project, timeout) = {
        let c = s.core.lock().await;
        (
            c.sessions.get(&id).ok_or_else(missing)?.project.clone(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let file = s
        .file_io
        .run(timeout, move |_| -> Result<std::fs::File> {
            let path = tools::output_path(&project)?;
            tools::open_regular_file(&path)
        })
        .await?;
    let body = Body::from_stream(s.file_io.stream(file, timeout)?);
    Ok((
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}
#[derive(Deserialize)]
struct DirectoryQuery {
    path: Option<PathBuf>,
}
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
async fn directories(State(s): State<WebState>, Query(query): Query<DirectoryQuery>) -> Api {
    let timeout = Duration::from_secs(s.core.lock().await.config.tool_timeout_secs);
    let result = s.file_io.run(timeout, move |cancel| -> Result<Value> {
        let path = query
            .path
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?;
        tools::utf8_path(&path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            if let Ok(entry) = entry
                && entry.file_type().is_ok_and(|t| t.is_dir())
            {
                let entry_path = entry.path();
                if entry_path.to_str().is_none() {
                    continue;
                }
                if entries.len() == MAX_DIRECTORY_ENTRIES {
                    bail!("directory_listing_capacity: 하위 폴더가 너무 많습니다. 원하는 폴더 경로를 직접 입력하세요.");
                }
                entries.push(json!({"name":entry.file_name().to_string_lossy(),"path":entry_path}));
            }
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"path":path,"parent":path.parent(),"directories":entries}))
    })
    .await?;
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

async fn stdin_eof() {
    // stdin and EOF belong to the process. Restarting serve_app while stdin
    // is still open must not leave another thread waiting for the stdin lock.
    static CLOSED: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
        std::sync::OnceLock::new();
    let mut closed = CLOSED
        .get_or_init(|| {
            let (sender, _) = tokio::sync::watch::channel(false);
            let signal = sender.clone();
            // A native reader does not keep Tokio runtime shutdown waiting.
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
                signal.send_replace(true);
            });
            sender
        })
        .subscribe();
    let _ = closed.wait_for(|closed| *closed).await;
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
    // A dropped/aborted server must wake agent tasks too.
    let _stop_on_drop = state.stopping.clone().drop_guard();
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
    let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    crate::console::notice(crate::console::Target::Stdout, format!("MnemoArc: {url}\n"));
    // The listener is bound before launching the browser; failures leave a usable printed URL.
    let browser = open_browser.then(|| {
        let stopping = state.stopping.clone();
        tokio::spawn(async move {
            let result = crate::desktop::open_browser_managed(&url, &stopping).await;
            if let Err(error) = result
                && !stopping.is_cancelled()
            {
                crate::console::notice(
                    crate::console::Target::Stderr,
                    format!("브라우저를 열지 못했습니다: {error}. 직접 접속하세요: {url}\n"),
                );
            }
        })
    });
    let shutdown = state.clone();
    let listener = ManagedListener::new(listener, state.stopping.clone());
    tokio::join!(listener.serve(app_router(state, frontend)), async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = shutdown.stopping.cancelled() => {},
            _ = stdin_eof(), if shutdown_on_stdin => {},
        }
        shutdown.shutdown().await;
    });
    if let Some(browser) = browser {
        let _ = browser.await;
    }
    Ok(())
}

#[cfg(test)]
mod worker_wait_tests {
    use super::*;

    #[tokio::test]
    async fn session_events_identify_the_owner_without_invalidating_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let state = WebState::new(
            Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let first = state.core.lock().await.order[0].clone();
        let Json(created) = create_session(
            State(state.clone()),
            Json(NewSession {
                project: Project {
                    root: dir.path().into(),
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap();
        let second = created["id"].as_str().unwrap().to_owned();
        let mut events = state.events.subscribe();
        let _ = session_workflow(
            State(state.clone()),
            Path(second.clone()),
            Json(WorkflowSelection {
                workflow: "document_edit".into(),
            }),
        )
        .await
        .unwrap();
        let change = events.try_recv().unwrap();
        assert_eq!(change.session.as_deref(), Some(second.as_str()));
        assert!(!change.state);
        let Json(list) = state_get(State(state.clone())).await;
        assert_eq!(list["sessions"][0]["id"], first);
        assert_eq!(list["sessions"][0]["revision"], 0);
        assert_eq!(list["sessions"][1]["revision"], change.revision);
        let _ = close_session(State(state.clone()), Path(second.clone()))
            .await
            .unwrap();
        let change = events.try_recv().unwrap();
        assert_eq!(change.session.as_deref(), Some(second.as_str()));
        assert!(change.state);
        assert!(
            !state
                .core
                .lock()
                .await
                .session_revisions
                .contains_key(&second)
        );
    }

    #[tokio::test]
    async fn snapshots_refresh_the_sidebar_only_when_the_summary_changes() {
        let dir = tempfile::tempdir().unwrap();
        let state = WebState::new(
            Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let mut events = state.events.subscribe();
        let mut core = state.core.lock().await;
        let id = core.order[0].clone();
        let (commands, _) = mpsc::channel(1);
        core.running.insert(
            id.clone(),
            Running {
                cancel: CancellationToken::new(),
                commands,
                closing: false,
            },
        );
        let mut session = core.sessions[&id].session.clone();
        session.input_tokens += 100;
        publish_snapshot(
            &state,
            &mut core,
            Arc::new(session_view::Snapshot::new(session.clone())),
        );
        let change = events.try_recv().unwrap();
        assert_eq!(change.session.as_deref(), Some(id.as_str()));
        assert!(!change.state);
        session.status = "running".into();
        publish_snapshot(
            &state,
            &mut core,
            Arc::new(session_view::Snapshot::new(session)),
        );
        assert!(events.try_recv().unwrap().state);
    }

    #[tokio::test]
    async fn unfinished_request_bodies_time_out_while_the_server_is_running() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let state = WebState::new(
            Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let _stop_on_drop = state.stopping.clone().drop_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let managed = ManagedListener::new(listener, state.stopping.clone());
        let app = app_router(state.clone(), None);
        let server = tokio::spawn(managed.serve(app));
        let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
        stalled
            .write_all(format!("POST /api/sessions HTTP/1.1\r\nHost: {addr}\r\nx-mnemoarc-client: web\r\nContent-Type: application/json\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut interim = [0; 25];
        tokio::time::timeout(Duration::from_secs(2), stalled.read_exact(&mut interim))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        let mut response = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(2), stalled.read_to_end(&mut response)).await;
        tokio::time::resume();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let oversized = client
            .post(format!("http://{addr}/api/sessions"))
            .header("x-mnemoarc-client", "web")
            .json(&json!({"project":Project {
                root: dir.path().into(),
                purpose: "x".repeat(2 * 1024 * 1024),
                ..Default::default()
            }}))
            .send()
            .await
            .unwrap();
        let healthy = client
            .get(format!("http://{addr}/api/state"))
            .send()
            .await
            .unwrap();
        let sessions = state.core.lock().await.sessions.len();
        // Clean up even when the timeout check regresses.
        drop(stalled);
        state.stopping.cancel();
        server.abort();
        let _ = server.await;
        closed
            .expect("unfinished HTTP request retained its connection and body without a deadline")
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 408"), "{response:?}");
        assert_eq!(healthy.status(), StatusCode::OK);
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(sessions, 1, "partial request must not create a session");
    }

    #[tokio::test]
    async fn body_receipt_deadline_does_not_cut_off_a_long_model_connection_check() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let upstream = Router::new().route(
            "/chat/completions",
            post({
                let entered = entered.clone();
                move |Json(request): Json<Value>| {
                    let entered = entered.clone();
                    async move {
                        if request["stream"] != true {
                            entered.notify_one();
                            tokio::time::sleep(Duration::from_secs(40)).await;
                            return Json(json!({"choices":[{"message":{"content":"OK"},"finish_reason":"stop"}]})).into_response();
                        }
                        let choice = if request["tool_choice"]["function"]["name"] == "connection_echo" {
                            json!({"delta":{"tool_calls":[{"index":0,"id":"echo","function":{"name":"connection_echo","arguments":"{\"text\":\"OK\"}"}}]},"finish_reason":"tool_calls"})
                        } else {
                            json!({"delta":{"content":"OK"},"finish_reason":"stop"})
                        };
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[choice]})),
                        ).into_response()
                    }
                }
            }),
        );
        let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", provider.local_addr().unwrap());
        let provider = tokio::spawn(async move { axum::serve(provider, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            base_url,
            model: "gpt-4o".into(),
            model_context: Some(128000),
            api_key_env: "MNEMOARC_TEST_UNUSED_API_KEY".into(),
            disable_proxy: true,
            retries: 0,
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(
            config.clone(),
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let _stop_on_drop = state.stopping.clone().drop_guard();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let managed = ManagedListener::new(listener, state.stopping.clone());
        let app = app_router(state.clone(), None);
        let server = tokio::spawn(managed.serve(app));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = tokio::spawn(async move {
            client
                .post(format!("http://{addr}/api/check"))
                .header("x-mnemoarc-client", "web")
                .json(&json!({"config":config}))
                .send()
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        tokio::time::resume();
        let response = tokio::time::timeout(Duration::from_secs(3), request).await;
        state.stopping.cancel();
        server.abort();
        provider.abort();
        let _ = server.await;
        let _ = provider.await;
        let response = response.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = response.json::<Value>().await.unwrap();
        assert!(value["message"].as_str().unwrap().contains("succeeded"));
    }

    #[tokio::test]
    async fn closing_idle_sessions_releases_the_session_order_storage() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project {
            root: dir.path().into(),
            ..Default::default()
        };
        let state = WebState::new(
            Config {
                projects: vec![project.clone()],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        for _ in 0..128 {
            let _ = create_session(
                State(state.clone()),
                Json(NewSession {
                    project: project.clone(),
                }),
            )
            .await
            .unwrap();
        }
        let ids = state.core.lock().await.order.clone();
        for id in &ids[..ids.len() - 1] {
            let _ = close_session(State(state.clone()), Path(id.clone()))
                .await
                .unwrap();
        }
        {
            let core = state.core.lock().await;
            assert_eq!(core.order, ids[ids.len() - 1..]);
            assert!(
                core.order.capacity() <= 2,
                "closed sessions kept order slots"
            );
        }
        let _ = close_session(State(state.clone()), Path(ids.last().unwrap().clone()))
            .await
            .unwrap();
        let core = state.core.lock().await;
        assert!(core.sessions.is_empty());
        assert!(core.order.is_empty());
        assert_eq!(core.order.capacity(), 0);
    }

    #[tokio::test]
    async fn oversized_directory_listing_is_rejected_instead_of_retaining_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let state = WebState::new(
            Config::default(),
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            std::fs::create_dir(dir.path().join(index.to_string())).unwrap();
        }
        let error = directories(
            State(state.clone()),
            Query(DirectoryQuery {
                path: Some(dir.path().into()),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.starts_with("directory_listing_capacity:"));
        state.shutdown().await;
    }

    struct LifecycleProbe(tokio::sync::Notify);

    #[async_trait::async_trait]
    impl LlmClient for LifecycleProbe {
        async fn complete(
            &self,
            _: Value,
            _: &Config,
            cancel: CancellationToken,
            delta: mpsc::Sender<String>,
        ) -> Result<crate::llm::Completion> {
            delta.send("ongoing response".into()).await.unwrap();
            self.0.notify_one();
            cancel.cancelled().await;
            bail!("cancelled");
        }
    }

    #[tokio::test]
    async fn shutdown_request_releases_running_agents_without_an_explicit_shutdown_wait() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(LifecycleProbe(tokio::sync::Notify::new()));
        let state = WebState::new(
            Config {
                model: "gpt-4o".into(),
                model_context: Some(128000),
                source_answer_review: false,
                completion_review_enabled: false,
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            client.clone(),
        )
        .unwrap();
        let id = state.core.lock().await.order[0].clone();
        let core_refs = Arc::strong_count(&state.core);
        let session_refs = Arc::strong_count(&state.write_outcome_uncertain);
        let _ = run(
            State(state.clone()),
            Path(id.clone()),
            Json(RunInput {
                text: "Run until shutdown".into(),
                action: "chat".into(),
            }),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), client.0.notified())
            .await
            .unwrap();
        let _ = shutdown_request(State(state.clone())).await;
        let settled = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.core.lock().await.running.is_empty()
                    && Arc::strong_count(&state.core) == core_refs
                    && Arc::strong_count(&state.write_outcome_uncertain) == session_refs
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        state.shutdown().await;
        settled.expect("shutdown signal retained the agent, event pump and private session");
        let core = state.core.lock().await;
        assert!(core.streams.is_empty());
        assert_eq!(core.sessions[&id].status, "cancelled");
    }

    #[tokio::test]
    async fn repeated_running_session_closes_release_all_snapshots_and_web_owners() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project {
            root: dir.path().into(),
            ..Default::default()
        };
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            source_answer_review: false,
            completion_review_enabled: false,
            projects: vec![project.clone()],
            ..Default::default()
        };
        let client = Arc::new(LifecycleProbe(tokio::sync::Notify::new()));
        let state = WebState::new(config, dir.path().join("config.toml"), client.clone()).unwrap();
        let initial = state.core.lock().await.order[0].clone();
        let _ = close_session(State(state.clone()), Path(initial))
            .await
            .unwrap();
        let core_refs = Arc::strong_count(&state.core);
        let session_refs = Arc::strong_count(&state.write_outcome_uncertain);
        for _ in 0..16 {
            let Json(created) = create_session(
                State(state.clone()),
                Json(NewSession {
                    project: project.clone(),
                }),
            )
            .await
            .unwrap();
            let id = created["id"].as_str().unwrap().to_owned();
            let _ = run(
                State(state.clone()),
                Path(id.clone()),
                Json(RunInput {
                    text: "Run until closed".into(),
                    action: "chat".into(),
                }),
            )
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), client.0.notified())
                .await
                .unwrap();
            let _ = close_session(State(state.clone()), Path(id)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let empty = state.core.lock().await.running.is_empty();
                    if empty
                        && Arc::strong_count(&state.core) == core_refs
                        && Arc::strong_count(&state.write_outcome_uncertain) == session_refs
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("closing must drop every private session and event pump owner");
            let core = state.core.lock().await;
            assert!(core.sessions.is_empty());
            assert!(core.order.is_empty());
            assert_eq!(core.order.capacity(), 0);
            assert!(core.streams.is_empty());
        }
        state.shutdown().await;
    }

    struct SettlementProbe(tokio::sync::Notify, &'static str);

    #[async_trait::async_trait]
    impl LlmClient for SettlementProbe {
        async fn complete(
            &self,
            _: Value,
            _: &Config,
            cancel: CancellationToken,
            delta: mpsc::Sender<String>,
        ) -> Result<crate::llm::Completion> {
            delta.send("ongoing response".into()).await.unwrap();
            self.0.notify_one();
            match self.1 {
                "complete" => Ok(crate::llm::Completion {
                    text: "Completed while snapshot validation was queued.".into(),
                    ..Default::default()
                }),
                "panic" => panic!("simulated model panic with queued snapshots"),
                _ => {
                    cancel.cancelled().await;
                    bail!("cancelled");
                }
            }
        }
    }

    #[tokio::test]
    async fn stopped_runs_release_snapshots_while_file_workers_are_full() {
        for outcome in ["cancel", "close", "complete", "panic", "timeout"] {
            let closing = outcome == "close";
            let dir = tempfile::tempdir().unwrap();
            let client = Arc::new(SettlementProbe(tokio::sync::Notify::new(), outcome));
            let mut state = WebState::new(
                Config {
                    model: "gpt-4o".into(),
                    model_context: Some(128000),
                    source_answer_review: false,
                    completion_review_enabled: false,
                    tool_timeout_secs: 60,
                    run_timeout_secs: if outcome == "timeout" { 1 } else { 60 },
                    projects: vec![Project {
                        root: dir.path().into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                dir.path().join("config.toml"),
                client.clone(),
            )
            .unwrap();
            let workers = Arc::new(tokio::sync::Semaphore::new(1));
            state.file_io = FileIo::with_workers(workers.clone(), state.stopping.clone());
            let occupied = workers.acquire_owned().await.unwrap();
            let id = state.core.lock().await.order[0].clone();
            let core_refs = Arc::strong_count(&state.core);
            let session_refs = Arc::strong_count(&state.write_outcome_uncertain);
            let _ = run(
                State(state.clone()),
                Path(id.clone()),
                Json(RunInput {
                    text: "Run until stopped with pending snapshot validation".into(),
                    action: "chat".into(),
                }),
            )
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), client.0.notified())
                .await
                .unwrap();
            tokio::task::yield_now().await;
            if closing {
                let _ = close_session(State(state.clone()), Path(id.clone()))
                    .await
                    .unwrap();
            } else if outcome == "cancel" {
                let _ = cancel(State(state.clone()), Path(id.clone()))
                    .await
                    .unwrap();
            }
            let settled = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let core = state.core.lock().await;
                    let expected_refs = if closing {
                        session_refs - 1
                    } else {
                        session_refs
                    };
                    if core.running.is_empty()
                        && Arc::strong_count(&state.core) == core_refs
                        && Arc::strong_count(&state.write_outcome_uncertain) == expected_refs
                    {
                        assert!(core.streams.is_empty());
                        if closing {
                            assert!(core.sessions.is_empty());
                        } else {
                            let session = &core.sessions[&id];
                            match outcome {
                                "complete" => assert_eq!(session.status, "complete"),
                                "panic" | "timeout" => {
                                    assert_eq!(session.status, "blocked");
                                    assert!(session.last_error.as_deref().unwrap().starts_with(
                                        if outcome == "panic" {
                                            "model_worker_panic"
                                        } else {
                                            "run_timeout"
                                        }
                                    ));
                                }
                                _ => assert_eq!(session.status, "cancelled"),
                            }
                        }
                        break;
                    }
                    drop(core);
                    tokio::task::yield_now().await;
                }
            })
            .await;
            // Release the fixture before asserting so a regression leaves no
            // detached agent or event pump behind in this test's runtime.
            drop(occupied);
            state.shutdown().await;
            settled.expect("stopped run retained its slot, snapshot queue and web owners");
        }
    }

    #[tokio::test]
    async fn new_task_that_cannot_fit_metadata_preserves_the_previous_task() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            memory_bytes: 32 * 1024,
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(
            config,
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let id = state.core.lock().await.order[0].clone();
        state
            .core
            .lock()
            .await
            .session_mut(&id)
            .unwrap()
            .add_user("Existing task".into());
        let result = run(
            State(state.clone()),
            Path(id.clone()),
            Json(RunInput {
                text: "x".repeat(32 * 1024),
                action: "chat".into(),
            }),
        )
        .await;
        assert!(
            matches!(result, Err(ApiError(StatusCode::BAD_REQUEST, ref error)) if error.starts_with("session_metadata_capacity:"))
        );
        let core = state.core.lock().await;
        let session = &core.sessions[&id];
        assert_eq!(session.latest_request, "Existing task");
        assert_eq!(session.history.next_id, 1);
        assert_eq!(session.sources.len(), 1);
        assert!(core.running.is_empty());
        assert_eq!(core.revision, 0);
    }

    #[tokio::test]
    async fn new_task_at_history_capacity_is_rejected_without_mutating_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(
            config,
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let id = state.core.lock().await.order[0].clone();
        let (bytes, next_id, sources, request, revision) = {
            let mut core = state.core.lock().await;
            let session = core.session_mut(&id).unwrap();
            session.add_user("Preserve the current task".into());
            session.history.push(
                vec![json!({"role":"assistant","content":"x".repeat(4096)})],
                true,
            );
            session.config.history_bytes = session.history.bytes();
            (
                session.history.bytes(),
                session.history.next_id,
                session.sources.len(),
                session.latest_request.clone(),
                core.revision,
            )
        };
        for _ in 0..3 {
            let result = run(
                State(state.clone()),
                Path(id.clone()),
                Json(RunInput {
                    text: "Another task".into(),
                    action: "chat".into(),
                }),
            )
            .await;
            assert!(
                matches!(result, Err(ApiError(StatusCode::BAD_REQUEST, ref error)) if error.starts_with("history_capacity"))
            );
            let core = state.core.lock().await;
            let session = &core.sessions[&id];
            assert_eq!(session.history.bytes(), bytes);
            assert_eq!(session.history.next_id, next_id);
            assert_eq!(session.sources.len(), sources);
            assert_eq!(session.latest_request, request);
            assert_eq!(core.revision, revision);
            assert!(core.running.is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_credential_save_restores_config_content_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let path = dir.path().join("config.toml");
        let state = WebState::new(config.clone(), path.clone(), Arc::new(OpenAiClient)).unwrap();
        let original = b"original settings";
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::create_dir(credential_path(&path)).unwrap();

        let result = settings(
            State(state.clone()),
            Json(Settings {
                config,
                api_key: Some("test-key".into()),
                credential_mode: "save".into(),
            }),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(state.core.lock().await.revision, 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn directory_listing_skips_non_utf8_names_and_rejects_non_utf8_targets() {
        use std::{
            ffi::OsString,
            os::unix::{ffi::OsStringExt, fs::symlink},
        };

        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join(OsString::from_vec(b"folder-\xff".to_vec()));
        std::fs::create_dir(&invalid).unwrap();
        std::fs::create_dir(dir.path().join("valid")).unwrap();
        symlink(&invalid, dir.path().join("alias")).unwrap();
        let state = WebState::new(
            Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let listed = directories(
            State(state.clone()),
            Query(DirectoryQuery {
                path: Some(dir.path().into()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(listed["directories"].as_array().unwrap().len(), 1);
        assert_eq!(listed["directories"][0]["name"], "valid");
        let error = directories(
            State(state.clone()),
            Query(DirectoryQuery {
                path: Some(dir.path().join("alias")),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            error.1.starts_with("unsupported_non_utf8_path:"),
            "{error:?}"
        );
        let result = create_session(
            State(state.clone()),
            Json(NewSession {
                project: Project {
                    root: dir.path().join("alias"),
                    ..Default::default()
                },
            }),
        )
        .await;
        assert!(
            matches!(result, Err(ApiError(StatusCode::BAD_REQUEST, ref error)) if error.starts_with("unsupported_non_utf8_path:"))
        );
        assert_eq!(state.core.lock().await.sessions.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_save_rejects_a_fifo_without_blocking_other_api_requests() {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(config.clone(), path.clone(), Arc::new(OpenAiClient)).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        // Keep a writer open so an unchecked read would wait indefinitely.
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let save_state = state.clone();
        let save = tokio::spawn(async move {
            settings(
                State(save_state),
                Json(Settings {
                    config,
                    api_key: Some("test-key".into()),
                    credential_mode: "save".into(),
                }),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let responsive =
            tokio::time::timeout(Duration::from_millis(200), state_get(State(state.clone()))).await;
        // Release a regressed reader before asserting, so the test cannot hang.
        drop(writer);
        let saved = tokio::time::timeout(Duration::from_secs(2), save)
            .await
            .unwrap()
            .unwrap();
        assert!(
            responsive.is_ok(),
            "settings I/O retained the shared session lock"
        );
        assert!(
            matches!(saved, Err(ApiError(StatusCode::BAD_REQUEST, ref error)) if error.starts_with("unsupported_file_type:")),
            "{saved:?}"
        );
        assert!(!std::fs::metadata(path).unwrap().is_file());
        assert_eq!(state.core.lock().await.revision, 0);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_project_paths_are_rejected_before_creating_web_state() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let dir = tempfile::tempdir().unwrap();
        for field in ["root", "output"] {
            let invalid = PathBuf::from(OsString::from_vec(b"invalid-\xff".to_vec()));
            let mut project = Project {
                root: dir.path().into(),
                ..Default::default()
            };
            if field == "root" {
                project.root = invalid;
            } else {
                project.output = invalid;
            }
            let config = Config {
                projects: vec![project],
                ..Default::default()
            };
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .starts_with("unsupported_non_utf8_path:")
            );
            let error = WebState::new(
                config,
                dir.path().join("config.toml"),
                Arc::new(OpenAiClient),
            )
            .err()
            .expect("unserializable project was accepted");
            assert!(error.to_string().starts_with("unsupported_non_utf8_path:"));
        }
    }

    #[tokio::test]
    async fn queued_settings_save_keeps_other_requests_responsive_and_cancels_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut state =
            WebState::new(config.clone(), path.clone(), Arc::new(OpenAiClient)).unwrap();
        let workers = Arc::new(tokio::sync::Semaphore::new(1));
        let occupied = workers.clone().acquire_owned().await.unwrap();
        state.file_io = FileIo::with_workers(workers, state.stopping.clone());
        let save_state = state.clone();
        let save = tokio::spawn(async move {
            settings(
                State(save_state),
                Json(Settings {
                    config,
                    api_key: Some("test-key".into()),
                    credential_mode: "save".into(),
                }),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.settings_gate.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let current =
            tokio::time::timeout(Duration::from_millis(200), state_get(State(state.clone())))
                .await
                .unwrap()
                .0;
        assert_eq!(current["revision"], 0);
        assert!(!save.is_finished());
        let id = current["sessions"][0]["id"].as_str().unwrap().to_owned();
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            close_session(State(state.clone()), Path(id)),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_millis(200), state.shutdown())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), save)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, _))
        ));
        drop(occupied);
        assert!(!path.exists());
        assert!(!credential_path(&path).exists());
        assert!(state.settings_gate.try_lock().is_ok());
    }

    #[tokio::test]
    async fn concurrent_settings_saves_preserve_credentials_and_match_persisted_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(config.clone(), path.clone(), Arc::new(OpenAiClient)).unwrap();
        let mut saves = tokio::task::JoinSet::new();
        for index in 0..6 {
            let state = state.clone();
            let mut config = config.clone();
            config.api_key_env = format!("SETTINGS_TEST_{index}");
            config.request_timeout_secs = 20 + index;
            saves.spawn(async move {
                settings(
                    State(state),
                    Json(Settings {
                        config,
                        api_key: Some(format!("key-{index}")),
                        credential_mode: "save".into(),
                    }),
                )
                .await
            });
        }
        while let Some(result) = saves.join_next().await {
            let _ = result.unwrap().unwrap();
        }
        let persisted = Config::load(&path, &BTreeMap::new()).unwrap();
        let credentials: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(credential_path(&path)).unwrap()).unwrap();
        let core = state.core.lock().await;
        assert!(same_config(&core.config, &persisted));
        assert_eq!(core.revision, 6);
        assert_eq!(credentials.len(), 6);
        assert_eq!(core.credentials.len(), 6);
        for index in 0..6 {
            assert_eq!(
                credentials[&format!("SETTINGS_TEST_{index}")],
                format!("key-{index}")
            );
        }
    }

    #[tokio::test]
    async fn queued_project_validation_cannot_overwrite_a_newer_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = WebState::new(
            Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let (id, mut project) = {
            let c = state.core.lock().await;
            let id = c.order[0].clone();
            (id.clone(), c.sessions[&id].project.clone())
        };
        let original_purpose = project.purpose.clone();
        project.purpose = "Pending project update".into();
        let workers = Arc::new(tokio::sync::Semaphore::new(1));
        let occupied = workers.clone().acquire_owned().await.unwrap();
        state.file_io = FileIo::with_workers(workers, state.stopping.clone());
        let update_state = state.clone();
        let update_id = id.clone();
        let update = tokio::spawn(async move {
            session_project(State(update_state), Path(update_id), Json(project)).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if Arc::strong_count(&state.core.lock().await.sessions[&id]) > 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            session_workflow(
                State(state.clone()),
                Path(id.clone()),
                Json(WorkflowSelection {
                    workflow: "answer".into(),
                }),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!update.is_finished());
        drop(occupied);
        let result = tokio::time::timeout(Duration::from_secs(2), update)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(ApiError(StatusCode::CONFLICT, _))));
        let core = state.core.lock().await;
        assert_eq!(core.sessions[&id].workflow_mode, "answer");
        assert_eq!(core.sessions[&id].project.purpose, original_purpose);
        assert_eq!(core.revision, 1);
    }

    #[test]
    fn web_state_rejects_extreme_timeouts_before_any_api_can_create_a_timer() {
        let dir = tempfile::tempdir().unwrap();
        for field in [
            "request_timeout_secs",
            "tool_timeout_secs",
            "run_timeout_secs",
        ] {
            let mut config = Config {
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            match field {
                "request_timeout_secs" => config.request_timeout_secs = u64::MAX,
                "tool_timeout_secs" => config.tool_timeout_secs = u64::MAX,
                "run_timeout_secs" => config.run_timeout_secs = u64::MAX,
                _ => unreachable!(),
            }
            let error = WebState::new(
                config,
                dir.path().join("config.toml"),
                Arc::new(OpenAiClient),
            )
            .err()
            .expect("unsupported timeout was accepted");
            assert!(error.to_string().contains("clock range"));
        }
    }

    #[tokio::test]
    async fn project_cannot_change_after_its_history_was_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let replacement = dir.path().join("replacement");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        let config = Config {
            projects: vec![Project {
                root: original.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(
            config,
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let id = state.core.lock().await.order[0].clone();
        {
            let mut core = state.core.lock().await;
            let session = core.session_mut(&id).unwrap();
            session.add_user("Investigate the original project".into());
            for bundle in &mut session.history.bundles {
                bundle.active = false;
                bundle.reviewed = true;
            }
            session.history.prune(0).unwrap();
            assert!(session.history.bundles.is_empty());
            assert_eq!(session.history.pruned_through, Some(1));
        }
        let project_id = state.core.lock().await.sessions[&id].project.id.clone();
        let result = session_project(
            State(state.clone()),
            Path(id.clone()),
            Json(Project {
                id: project_id,
                root: replacement,
                ..Default::default()
            }),
        )
        .await;
        assert!(matches!(result, Err(ApiError(StatusCode::CONFLICT, _))));
        assert_eq!(
            state.core.lock().await.sessions[&id].project.root,
            original.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn successive_pending_settings_preserve_the_latest_credential_choice() {
        for mode in ["session", "clear"] {
            let dir = tempfile::tempdir().unwrap();
            let config = Config {
                api_key: Some(Secret("old-test-key".into())),
                projects: vec![Project {
                    root: dir.path().into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let state = WebState::new(
                config.clone(),
                dir.path().join("config.toml"),
                Arc::new(OpenAiClient),
            )
            .unwrap();
            let id = state.core.lock().await.order[0].clone();
            let (commands, mut receiver) = mpsc::channel(16);
            state.core.lock().await.running.insert(
                id.clone(),
                Running {
                    cancel: CancellationToken::new(),
                    commands,
                    closing: false,
                },
            );
            let first = session_settings(
                State(state.clone()),
                Path(id.clone()),
                Json(Settings {
                    config: config.clone(),
                    api_key: Some("new-test-key".into()),
                    credential_mode: mode.into(),
                }),
            )
            .await;
            assert!(first.is_ok());
            let mut next = config.clone();
            next.request_timeout_secs = 37;
            let second = session_settings(
                State(state.clone()),
                Path(id.clone()),
                Json(Settings {
                    config: next,
                    api_key: None,
                    credential_mode: "keep".into(),
                }),
            )
            .await;
            assert!(second.is_ok());
            let expected = (mode == "session").then_some("new-test-key");
            let core = state.core.lock().await;
            let session = &core.sessions[&id];
            assert_eq!(
                session.config.api_key.as_ref().map(|key| key.0.as_str()),
                Some("old-test-key")
            );
            let pending = session.pending_config.as_ref().unwrap();
            assert_eq!(pending.request_timeout_secs, 37);
            assert_eq!(pending.api_key.as_ref().map(|key| key.0.as_str()), expected);
            receiver.try_recv().unwrap();
            let RunCommand::Configure(queued) = receiver.try_recv().unwrap() else {
                panic!("expected config")
            };
            assert_eq!(queued.api_key.as_ref().map(|key| key.0.as_str()), expected);
        }
    }

    #[tokio::test]
    async fn unresolved_write_blocks_new_web_runs() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            projects: vec![Project {
                root: dir.path().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = WebState::new(
            config,
            dir.path().join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap();
        let id = state.core.lock().await.order[0].clone();
        state.write_outcome_uncertain.store(true, Ordering::Release);
        let result = run(
            State(state),
            Path(id),
            Json(RunInput {
                text: "next task".into(),
                action: "chat".into(),
            }),
        )
        .await;
        assert!(matches!(result, Err(ApiError(StatusCode::CONFLICT, _))));
    }
}
