//! Idle-time document improvement. While no run is active, start one run on
//! the opted-in project whose output document has gone longest without work,
//! and close that session when its run ends (see `start_run`'s settlement).
use super::*;
use chrono::{DateTime, Utc};
use std::{
    collections::VecDeque,
    sync::{Mutex as StdMutex, PoisonError},
};

/// How often the scheduler decides whether to start a run.
const TICK: Duration = Duration::from_secs(60);
/// Automatic runs kept for the settings page; older ones are dropped.
const RUN_LOG_LIMIT: usize = 20;
const WORKFLOW: &str = "source_document";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ProjectTimes {
    /// When a run of any session of this project last ended.
    last_run_at: Option<DateTime<Utc>>,
    /// When an automatic run of this project was last started or attempted.
    last_auto_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct AutoRun {
    project_id: String,
    project_name: String,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    status: String,
    reason: String,
    error: Option<String>,
    input_tokens: usize,
    output_tokens: usize,
    rounds: usize,
    document_written: bool,
}

impl AutoRun {
    /// The closing record of an automatic session's only run.
    pub(super) fn finished(session: &Session, ended: DateTime<Utc>) -> Self {
        let record = session.run_history.back();
        Self {
            project_id: session.project.id.clone(),
            project_name: session.project.name.clone(),
            started_at: record.map_or(ended, |r| r.started_at),
            ended_at: record.map_or(ended, |r| r.ended_at),
            status: record.map_or_else(|| session.status.clone(), |r| r.status.clone()),
            reason: record.map(|r| r.reason.clone()).unwrap_or_default(),
            error: record
                .and_then(|r| r.error.clone())
                .or_else(|| session.last_error.clone()),
            input_tokens: record.map_or(0, |r| r.input_tokens),
            output_tokens: record.map_or(0, |r| r.output_tokens),
            rounds: record.map_or(0, |r| r.rounds),
            document_written: session.document_written,
        }
    }

    fn not_started(project: &Project, at: DateTime<Utc>, error: String) -> Self {
        Self {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            started_at: at,
            ended_at: at,
            status: "not_started".into(),
            reason: "start_failed".into(),
            error: Some(error),
            input_tokens: 0,
            output_tokens: 0,
            rounds: 0,
            document_written: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    projects: BTreeMap<String, ProjectTimes>,
    /// Automatic starts in the last 24 hours, for max_runs_per_day.
    attempts: Vec<DateTime<Utc>>,
    /// Newest last.
    runs: VecDeque<AutoRun>,
}

/// Schedule state kept beside the settings file, since sessions live only in
/// RAM: per-project work times, recent starts and a short run log. No
/// conversation or memory is stored.
pub(super) struct Automation {
    path: PathBuf,
    saved: StdMutex<Saved>,
    /// Serializes file writes; each write stores the state current when it runs.
    writer: StdMutex<()>,
}

impl Automation {
    pub(super) fn load(settings: &FsPath) -> Self {
        let path = settings.with_extension("automation.json");
        let saved = match path.try_exists() {
            Ok(true) => tools::read_text(&path)
                .and_then(|text| Ok(serde_json::from_str(&text)?))
                .unwrap_or_else(|error| {
                    // Losing the schedule only delays or repeats one run;
                    // it must not keep the app from starting.
                    crate::console::notice(
                        crate::console::Target::Stderr,
                        format!("자동 개선 기록을 읽지 못해 새로 시작합니다: {error}\n"),
                    );
                    Saved::default()
                }),
            _ => Saved::default(),
        };
        Self {
            path,
            saved: StdMutex::new(saved),
            writer: StdMutex::new(()),
        }
    }

    fn saved(&self) -> std::sync::MutexGuard<'_, Saved> {
        self.saved.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Recent automatic runs (newest first) and per-project times.
    pub(super) fn view(&self) -> Value {
        let saved = self.saved();
        json!({"runs":saved.runs.iter().rev().collect::<Vec<_>>(),"projects":saved.projects})
    }

    /// Record a finished run of any session; `auto_run` for automatic ones.
    /// Only the configured `projects` keep times, so sessions of unsaved or
    /// removed projects do not grow the file. `persist` then stores it.
    pub(super) fn note_run_end(
        &self,
        projects: &[Project],
        project_id: &str,
        ended: DateTime<Utc>,
        auto_run: Option<AutoRun>,
    ) {
        let mut saved = self.saved();
        saved
            .projects
            .retain(|id, _| projects.iter().any(|p| &p.id == id));
        if projects.iter().any(|p| p.id == project_id) {
            saved
                .projects
                .entry(project_id.to_owned())
                .or_default()
                .last_run_at = Some(ended);
        }
        if let Some(run) = auto_run {
            saved.runs.push_back(run);
            while saved.runs.len() > RUN_LOG_LIMIT {
                saved.runs.pop_front();
            }
        }
    }

    fn note_attempt(&self, project_id: &str, at: DateTime<Utc>) {
        let mut saved = self.saved();
        saved
            .projects
            .entry(project_id.to_owned())
            .or_default()
            .last_auto_at = Some(at);
        saved.attempts.push(at);
    }

    /// Per-project times and the starts within 24 hours of `now`.
    fn snapshot(&self, now: DateTime<Utc>) -> (BTreeMap<String, ProjectTimes>, usize) {
        let mut saved = self.saved();
        let day = chrono::Duration::days(1);
        saved
            .attempts
            .retain(|at| now.signed_duration_since(*at) < day);
        (saved.projects.clone(), saved.attempts.len())
    }

    /// Start writing the current state; the write runs even if the handle is
    /// dropped. Failures are reported and otherwise ignored: the in-memory
    /// state still prevents repeats until the app restarts.
    pub(super) fn persist(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let automation = self.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = automation.write() {
                crate::console::notice(
                    crate::console::Target::Stderr,
                    format!("자동 개선 기록을 저장하지 못했습니다: {error}\n"),
                );
            }
        })
    }

    fn write(&self) -> Result<()> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let text = serde_json::to_string(&*self.saved())?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(FsPath::new("."));
        std::fs::create_dir_all(parent)?;
        // A rename replaces whatever is at the path without opening it.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
}

/// The opted-in project to improve now: its output document exists and the
/// latest of the document's change, a run end and an automatic attempt is at
/// least `stale` old. The oldest wins.
fn stalest(
    candidates: Vec<(Project, String)>,
    times: &BTreeMap<String, ProjectTimes>,
    now: DateTime<Utc>,
    stale: chrono::Duration,
) -> Option<(Project, String)> {
    candidates
        .into_iter()
        .filter_map(|(project, prompt)| {
            let document = tools::output_path(&project).ok()?;
            let metadata = std::fs::metadata(&document).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let changed = metadata.modified().ok().map(DateTime::<Utc>::from);
            let recorded = times.get(&project.id);
            let last = [
                changed,
                recorded.and_then(|t| t.last_run_at),
                recorded.and_then(|t| t.last_auto_at),
            ]
            .into_iter()
            .flatten()
            .max();
            let due = last.is_none_or(|last| now.signed_duration_since(last) >= stale);
            due.then_some((last, project, prompt))
        })
        .min_by_key(|(last, ..)| *last)
        .map(|(_, project, prompt)| (project, prompt))
}

/// One scheduling decision at `now`; returns the session it started.
pub(super) async fn tick(s: &WebState, now: DateTime<Utc>) -> Option<String> {
    let config = {
        let c = s.core.lock().await;
        let automation = &c.config.automation;
        let idle = chrono::Duration::minutes(automation.idle_minutes as i64);
        if !automation.enabled
            || s.stopping.is_cancelled()
            || !c.running.is_empty()
            || s.write_outcome_uncertain.load(Ordering::Acquire)
            || now.signed_duration_since(c.idle_since) < idle
            || c.config.runnable().is_err()
        {
            return None;
        }
        c.config.clone()
    };
    let automation = &config.automation;
    let (times, attempts) = s.automation.snapshot(now);
    if attempts >= automation.max_runs_per_day {
        return None;
    }
    let candidates: Vec<_> = config
        .projects
        .iter()
        .filter_map(|p| Some((p.clone(), p.automation_prompt(automation)?.to_owned())))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let stale = chrono::Duration::days(automation.stale_days as i64);
    let (chosen, _) = s
        .file_io
        .run_queued(Duration::from_secs(config.tool_timeout_secs), move |_| {
            Ok(stalest(candidates, &times, now, stale))
        })
        .await
        .ok()
        .flatten()?;
    let (project, prompt, projects) = {
        // The document checks can take a while: a run, a switched-off
        // setting or a changed project in the meantime wins over the choice.
        let c = s.core.lock().await;
        let automation = &c.config.automation;
        let project = c.config.projects.iter().find(|p| p.id == chosen.id)?;
        let prompt = project.automation_prompt(automation)?.to_owned();
        let idle = chrono::Duration::minutes(automation.idle_minutes as i64);
        if !automation.enabled
            || !c.running.is_empty()
            || s.stopping.is_cancelled()
            || now.signed_duration_since(c.idle_since) < idle
        {
            return None;
        }
        // Recorded first, so a failed or interrupted run waits another
        // stale_days instead of starting again on the next tick.
        s.automation.note_attempt(&project.id, now);
        (project.clone(), prompt, c.config.projects.clone())
    };
    let _ = s.automation.persist().await;
    match start(s, project.clone(), prompt).await {
        Ok(id) => Some(id),
        Err(error) => {
            if !s.stopping.is_cancelled() {
                s.automation.note_run_end(
                    &projects,
                    &project.id,
                    now,
                    Some(AutoRun::not_started(&project, now, error.1)),
                );
                let _ = s.automation.persist().await;
                let mut c = s.core.lock().await;
                changed(s, &mut c);
            }
            None
        }
    }
}

async fn start(s: &WebState, project: Project, prompt: String) -> Result<String, ApiError> {
    let id = open_session(s, project, WORKFLOW, true).await?;
    let input = RunInput {
        text: prompt,
        action: auto_action(),
    };
    if let Err(error) = start_run(s, id.clone(), input, true).await {
        let mut c = s.core.lock().await;
        if !c.running.contains_key(&id) {
            c.remove_session(&id);
            session_changed(s, &mut c, &id, true);
        }
        return Err(error);
    }
    Ok(id)
}

/// The server's scheduler; it ends when the app stops.
pub(super) fn spawn(s: WebState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = s.stopping.cancelled() => break,
                _ = interval.tick() => {}
            }
            tick(&s, Utc::now()).await;
        }
    })
}
