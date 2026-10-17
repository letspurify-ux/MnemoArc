//! Idle-time document improvement. While no run is active, start one run on
//! the opted-in project whose output document has gone longest without work,
//! and close that session when its run ends (see `start_run`'s settlement).
use super::*;
use chrono::{DateTime, Utc};
use std::{
    collections::VecDeque,
    sync::{
        Mutex as StdMutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

impl Saved {
    /// Fold in what another process sharing these settings recorded: the
    /// later time of each project, every start and the newest runs.
    fn merge(&mut self, other: Saved) {
        for (id, times) in other.projects {
            let mine = self.projects.entry(id).or_default();
            mine.last_run_at = mine.last_run_at.max(times.last_run_at);
            mine.last_auto_at = mine.last_auto_at.max(times.last_auto_at);
        }
        self.attempts.extend(other.attempts);
        self.attempts.sort();
        self.attempts.dedup();
        // Only the scheduler counts starts (see snapshot); a process waiting
        // for the lease merges every minute and must not keep old ones.
        let day = chrono::Duration::days(1);
        let now = Utc::now();
        self.attempts
            .retain(|at| now.signed_duration_since(*at) < day);
        for run in other.runs {
            if !self.runs.contains(&run) {
                self.runs.push_back(run);
            }
        }
        self.runs.make_contiguous().sort_by_key(|run| run.ended_at);
        while self.runs.len() > RUN_LOG_LIMIT {
            self.runs.pop_front();
        }
    }
}

/// Which process schedules for the settings beside the lease file. Two app
/// processes may share one settings folder (the second takes another port);
/// each scheduling on its own would improve the same document twice.
enum Lease {
    /// Not taken: automatic improvement is off here, or another process
    /// holds the lease.
    Free,
    /// This process holds the exclusive lock on the lease file.
    Held { _lock: std::fs::File },
    /// The folder or file system offers no lock; this process schedules on
    /// its own, as a single process would.
    Unlocked,
}

/// Schedule state kept beside the settings file, since sessions live only in
/// RAM: per-project work times, recent starts and a short run log. No
/// conversation or memory is stored.
pub(super) struct Automation {
    settings: PathBuf,
    path: PathBuf,
    lease_path: PathBuf,
    saved: StdMutex<Saved>,
    /// Serializes file writes; each write stores the state current when it runs.
    writer: StdMutex<()>,
    /// Only the lease holder schedules and writes the file.
    lease: StdMutex<Lease>,
    /// Writes started and not yet finished; shutdown waits for them.
    pending: Arc<AtomicUsize>,
    /// A scheduler file operation (lease or document checks) still running.
    /// A stuck folder keeps its worker past the timeout; the next tick must
    /// not take another from the app's shared file workers.
    checking: Arc<AtomicBool>,
    /// An activity mark still being written, for the same reason.
    marking: Arc<AtomicBool>,
}

/// Holds one of the flags above until the operation's thread finishes.
pub(super) struct Busy(Arc<AtomicBool>);

impl Busy {
    fn begin(flag: &Arc<AtomicBool>) -> Option<Self> {
        (!flag.swap(true, AtomicOrdering::AcqRel)).then(|| Self(flag.clone()))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, AtomicOrdering::Release);
    }
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
            settings: settings.to_path_buf(),
            path,
            lease_path: settings.with_extension("automation.lock"),
            saved: StdMutex::new(saved),
            writer: StdMutex::new(()),
            lease: StdMutex::new(Lease::Free),
            pending: Arc::default(),
            checking: Arc::default(),
            marking: Arc::default(),
        }
    }

    fn saved(&self) -> std::sync::MutexGuard<'_, Saved> {
        self.saved.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lease(&self) -> std::sync::MutexGuard<'_, Lease> {
        self.lease.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn holds_lease(&self) -> bool {
        !matches!(*self.lease(), Lease::Free)
    }

    fn locked(&self) -> bool {
        matches!(*self.lease(), Lease::Held { .. })
    }

    /// Let another process sharing these settings schedule.
    fn release_lease(&self) {
        *self.lease() = Lease::Free;
    }

    /// Take the scheduler lease if no other process holds it. Taking it
    /// over merges and stores what the previous holder recorded; without
    /// it, the records shown here are refreshed from the file. Without a
    /// lock to take, this process schedules alone but tries the lock again
    /// on every call, so a passing error does not end the coordination.
    /// Blocking.
    fn take_lease(&self) -> bool {
        if self.locked() {
            return true;
        }
        // The file work runs outside the mutex: a slow folder must not hold
        // up the scheduler's own check of the lease.
        let taken = match self.lock_lease_file() {
            Ok(Some(lock)) => Lease::Held { _lock: lock },
            Ok(None) => {
                self.release_lease();
                self.merge_file();
                return false;
            }
            Err(_) => Lease::Unlocked,
        };
        {
            let mut lease = self.lease();
            let was_free = matches!(*lease, Lease::Free);
            if matches!(*lease, Lease::Held { .. }) {
                return true;
            }
            *lease = taken;
            if !was_free {
                return true;
            }
        }
        self.merge_file();
        if let Err(error) = self.write() {
            crate::console::notice(
                crate::console::Target::Stderr,
                format!("자동 개선 기록을 저장하지 못했습니다: {error}\n"),
            );
        }
        true
    }

    /// The lease file; `create` makes it (and its folder) when missing.
    fn open_lease_file(&self, create: bool) -> std::io::Result<std::fs::File> {
        if create
            && let Some(parent) = self
                .lease_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(create).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A FIFO at the path must not block the open waiting for a reader.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let new = create && !self.lease_path.try_exists().unwrap_or(true);
        let file = options.open(&self.lease_path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("lease path is not a regular file"));
        }
        if new {
            // Its time is the activity mark: creating the file is no work,
            // and must not add an idle period after switching on.
            let _ = file.set_modified(std::time::SystemTime::UNIX_EPOCH);
        }
        Ok(file)
    }

    /// The exclusive lock on the lease file, or None while another process
    /// holds it.
    fn lock_lease_file(&self) -> std::io::Result<Option<std::fs::File>> {
        let file = self.open_lease_file(true)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    /// Merge the records another process wrote. An unreadable file is left
    /// for the lease holder's next write to replace.
    fn merge_file(&self) {
        if !self.path.try_exists().unwrap_or(false) {
            return;
        }
        if let Ok(stored) =
            tools::read_text(&self.path).and_then(|text| Ok(serde_json::from_str::<Saved>(&text)?))
        {
            self.saved().merge(stored);
        }
    }

    /// Mark work at `at` for whichever process schedules: the lease file's
    /// modification time is the latest run start, run end or heartbeat of a
    /// running process sharing these settings. It never moves back, and
    /// without a lease file no process schedules to read it.
    pub(super) fn mark_activity(self: &Arc<Self>, at: DateTime<Utc>) {
        let Some(busy) = Busy::begin(&self.marking) else {
            return;
        };
        let automation = self.clone();
        tokio::task::spawn_blocking(move || {
            let _busy = busy;
            if automation.activity().is_some_and(|last| last >= at) {
                return;
            }
            if let Ok(file) = automation.open_lease_file(false) {
                let _ = file.set_modified(at.into());
            }
        });
    }

    /// The latest work any process sharing these settings marked. Blocking.
    fn activity(&self) -> Option<DateTime<Utc>> {
        let modified = std::fs::metadata(&self.lease_path).and_then(|m| m.modified());
        modified.ok().map(DateTime::from)
    }

    /// What the settings file says about running `project` now: Some of its
    /// prompt, or Some(None) when automation or the project is off there or
    /// the project was removed. Another process sharing the file may have
    /// changed it, which this process's memory does not show. None when the
    /// file cannot tell (missing, unreadable, or keeping no project
    /// identities that `known` has): the settings in memory decide. Blocking.
    fn prompt_on_disk(&self, project: &str, known: &[String]) -> Option<Option<String>> {
        if !self.settings.try_exists().unwrap_or(false) {
            return None;
        }
        let settings = crate::config::Config::load(&self.settings, &BTreeMap::new()).ok()?;
        if !settings.automation.enabled {
            return Some(None);
        }
        match settings.projects.iter().find(|p| p.id == project) {
            Some(saved) => Some(
                saved
                    .automation_prompt(&settings.automation)
                    .map(str::to_owned),
            ),
            None if settings.projects.iter().any(|p| known.contains(&p.id)) => Some(None),
            None => None,
        }
    }

    /// Whether every started write has finished.
    pub(super) fn written(&self) -> bool {
        self.pending.load(AtomicOrdering::Acquire) == 0
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
    /// dropped, and shutdown waits for it. Failures are reported and
    /// otherwise ignored: the in-memory state still prevents repeats until
    /// the app restarts.
    pub(super) fn persist(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        struct Finished(Arc<AtomicUsize>);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.fetch_sub(1, AtomicOrdering::AcqRel);
            }
        }
        self.pending.fetch_add(1, AtomicOrdering::AcqRel);
        let finished = Finished(self.pending.clone());
        let automation = self.clone();
        tokio::task::spawn_blocking(move || {
            let _finished = finished;
            if let Err(error) = automation.write() {
                crate::console::notice(
                    crate::console::Target::Stderr,
                    format!("자동 개선 기록을 저장하지 못했습니다: {error}\n"),
                );
            }
        })
    }

    /// Store the current state if this process holds the lease; another
    /// process's records are merged when this one takes the lease over.
    fn write(&self) -> Result<()> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.holds_lease() {
            return Ok(());
        }
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
    let timeout = {
        let c = s.core.lock().await;
        if !c.config.automation.enabled {
            // Another process sharing these settings may schedule instead,
            // once this one has no automatic run left to finish.
            if !c.running.values().any(|r| r.automatic) {
                s.automation.release_lease();
            }
            return None;
        }
        // A heartbeat, so a scheduler in another process sees a long run.
        if !c.running.is_empty() {
            s.automation.mark_activity(now);
        }
        Duration::from_secs(c.config.tool_timeout_secs)
    };
    if s.stopping.is_cancelled() {
        return None;
    }
    if !s.automation.locked() {
        let busy = Busy::begin(&s.automation.checking)?;
        let automation = s.automation.clone();
        let taken = s
            .file_io
            .run_queued(timeout, move |_| {
                let _busy = busy;
                Ok(automation.take_lease())
            })
            .await;
        if !taken.unwrap_or(false) {
            return None;
        }
    }
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
    let idle = chrono::Duration::minutes(automation.idle_minutes as i64);
    let known: Vec<_> = config.projects.iter().map(|p| p.id.clone()).collect();
    let marks = s.automation.clone();
    let busy = Busy::begin(&s.automation.checking)?;
    let (chosen, prompt) = s
        .file_io
        .run_queued(Duration::from_secs(config.tool_timeout_secs), move |_| {
            let _busy = busy;
            // Work in another process sharing these settings counts too.
            if marks
                .activity()
                .is_some_and(|at| now.signed_duration_since(at) < idle)
            {
                return Ok(None);
            }
            let Some((project, prompt)) = stalest(candidates, &times, now, stale) else {
                return Ok(None);
            };
            // Switched off, opted out or reworded in another window: the
            // settings file says so.
            let prompt = match marks.prompt_on_disk(&project.id, &known) {
                Some(Some(saved)) => saved,
                Some(None) => return Ok(None),
                None => prompt,
            };
            Ok(Some((project, prompt)))
        })
        .await
        .ok()
        .flatten()?;
    let (project, projects) = {
        // The document checks can take a while: a run, a switched-off
        // setting or a changed project in the meantime wins over the choice.
        let c = s.core.lock().await;
        let automation = &c.config.automation;
        let project = c.config.projects.iter().find(|p| p.id == chosen.id)?;
        project.automation_prompt(automation)?;
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
        (project.clone(), c.config.projects.clone())
    };
    // A stuck folder must not keep the app from stopping.
    tokio::select! {
        _ = s.stopping.cancelled() => return None,
        _ = s.automation.persist() => {}
    }
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
                // Written in the background; shutdown waits a bounded time.
                drop(s.automation.persist());
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
            // A panic in one decision must not end the scheduler for the
            // rest of the app's life.
            let _ = std::panic::AssertUnwindSafe(tick(&s, Utc::now()))
                .catch_unwind()
                .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ended: i64) -> AutoRun {
        let at = DateTime::from_timestamp(ended, 0).unwrap();
        AutoRun {
            project_id: "p".into(),
            project_name: "p".into(),
            started_at: at,
            ended_at: at,
            status: "complete".into(),
            reason: "complete".into(),
            error: None,
            input_tokens: 0,
            output_tokens: 0,
            rounds: 0,
            document_written: false,
        }
    }

    #[test]
    fn only_one_scheduler_file_operation_runs_at_a_time() {
        let flag = Arc::default();
        let first = Busy::begin(&flag).unwrap();
        assert!(Busy::begin(&flag).is_none());
        drop(first);
        assert!(Busy::begin(&flag).is_some());
    }

    #[test]
    fn merging_keeps_later_times_recent_starts_and_the_newest_runs() {
        let now = Utc::now();
        let hour = chrono::Duration::hours(1);
        let mut mine = Saved::default();
        mine.projects.insert(
            "a".into(),
            ProjectTimes {
                last_run_at: Some(now - hour),
                last_auto_at: None,
            },
        );
        mine.attempts = vec![now - hour];
        mine.runs = (0..15).map(run).collect();
        let mut other = Saved::default();
        other.projects.insert(
            "a".into(),
            ProjectTimes {
                last_run_at: Some(now - hour * 2),
                last_auto_at: Some(now),
            },
        );
        other.attempts = vec![now - hour, now - chrono::Duration::days(2)];
        other.runs = (10..25).map(run).collect();
        mine.merge(other);
        assert_eq!(mine.projects["a"].last_run_at, Some(now - hour));
        assert_eq!(mine.projects["a"].last_auto_at, Some(now));
        // Shared starts count once; a day-old one no longer counts.
        assert_eq!(mine.attempts, vec![now - hour]);
        let ended: Vec<_> = mine.runs.iter().map(|r| r.ended_at.timestamp()).collect();
        assert_eq!(ended, (5..25).collect::<Vec<_>>());
    }
}
