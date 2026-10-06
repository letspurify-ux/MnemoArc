use super::*;
use std::{ops::Deref, sync::OnceLock};

/// Readers retain an immutable snapshot without copying the complete history
/// under the workspace lock. Each snapshot computes display byte counts once.
#[derive(Clone)]
pub(super) struct Snapshot {
    pub(super) session: Session,
    usage: OnceLock<(usize, usize)>,
}

impl Snapshot {
    pub(super) fn new(session: Session) -> Self {
        Self {
            session,
            usage: OnceLock::new(),
        }
    }

    pub(super) fn session_mut(&mut self) -> &mut Session {
        self.usage.take();
        &mut self.session
    }

    fn usage(&self) -> (usize, usize) {
        *self
            .usage
            .get_or_init(|| (self.memory.bytes(), self.history.bytes()))
    }
}

impl Deref for Snapshot {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.session
    }
}

pub(super) fn revalidated(
    original: Arc<Snapshot>,
    cancel: &CancellationToken,
    checks: &FileChecks,
) -> Result<Arc<Snapshot>> {
    let freshness =
        tools::inspect_freshness_with(&original, cancel, |path| checks.hash(path, cancel))?;
    if !freshness.changed() {
        return Ok(original);
    }
    let mut session = original.session.clone();
    freshness.apply(&mut session);
    Ok(Arc::new(Snapshot::new(session)))
}

pub(super) enum Read {
    Page(Page),
    Memory(String),
}

pub(super) async fn read(s: WebState, id: String, request: Read) -> Api {
    let (original, revision, stream, timeout) = {
        let c = s.core.lock().await;
        (
            c.sessions.get(&id).ok_or_else(missing)?.clone(),
            c.revision,
            c.streams.get(&id).cloned(),
            Duration::from_secs(c.config.tool_timeout_secs),
        )
    };
    let is_page = matches!(&request, Read::Page(_));
    let input = original.clone();
    let checks = s.file_checks.clone();
    // Embedded routers can still read the settled sessions after shutdown.
    // Keep those reads available; the process-wide worker limit still applies.
    let reader = if s.stopping.is_cancelled() {
        FileIo::new(CancellationToken::new())
    } else {
        s.file_io.clone()
    };
    let (snapshot, mut value) = reader
        .run_queued(timeout, move |cancel| {
            let snapshot = revalidated(input, &cancel, &checks)?;
            let value = match request {
                Read::Page(page) => page_view(&snapshot, revision, stream, page),
                Read::Memory(memory) => json!(snapshot.memory.get(&memory)?),
            };
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            Ok((snapshot, value))
        })
        .await?;
    let mut c = s.core.lock().await;
    let current = c.sessions.get(&id).ok_or_else(missing)?;
    // Other sessions may have advanced while this worker was reading files.
    // Only this session's identity decides whether the validated copy can be
    // published; a late reader must not replace a newer snapshot or settings.
    if Arc::ptr_eq(current, &original) && !Arc::ptr_eq(&snapshot, &original) {
        c.sessions.insert(id.clone(), snapshot);
        session_changed(&s, &mut c, &id, false);
        if is_page {
            value["revision"] = json!(c.revision);
            value["stream"] = json!(c.streams.get(&id));
        }
    }
    if is_page {
        value["server_instance"] = json!(s.server_instance);
    }
    Ok(Json(value))
}

fn page_view(session: &Snapshot, revision: u64, stream: Option<String>, page: Page) -> Value {
    let mut bundles = session
        .history
        .bundles
        .iter()
        .rev()
        .filter(|bundle| page.before.is_none_or(|before| bundle.id < before))
        .take(page.limit.unwrap_or(50).clamp(1, 100))
        .collect::<Vec<_>>();
    bundles.reverse();
    let previous = bundles
        .first()
        .filter(|bundle| {
            session
                .history
                .bundles
                .iter()
                .any(|older| older.id < bundle.id)
        })
        .map(|bundle| bundle.id);
    let (memory_bytes, history_bytes) = session.usage();
    json!({"revision":revision,"id":session.id,"project":session.project,"config":session.config,"pending_config":session.pending_config,"credential_configured":OpenAiClient::has_key(&session.config),"status":session.status,"error":session.last_error,"run_history":session.run_history,"has_task":!session.latest_request.is_empty(),"can_resume":session.can_resume(),"original_request":session.original_request,"current_goal":session.latest_request,"task_amendments":session.task_amendments,"question_running":session.question.is_some(),"task":session.task,"workflow_mode":session.workflow_mode,"workflow_forbidden_tools":session.workflow_forbidden_tools(),"document_review":session.document_review,"completion_review":crate::tools::completion_review::view(session),"completion_gaps":session.completion_gaps,"run_guidance":session.run_guidance,"activity":session.activity,"continuation_pending":session.continuation.is_some(),"bundles":bundles,"previous":previous,"pruned_through":session.history.pruned_through,"stream":stream,"memories":session.memory.recent(session.config.memory_count),"active_tools":session.active_tools,"usage":{"input":session.input_tokens,"output":session.output_tokens,"cached":session.cached_tokens,"estimated":session.usage_incomplete,"context_estimated":crate::context::is_estimated(&session.config.model),"memory_bytes":memory_bytes,"history_bytes":history_bytes,"checkpoints":session.checkpoints_completed},"checkpoint":session.checkpoint})
}

/// Keep the authoritative agent snapshot even if validation is interrupted.
/// Every read validates again before exposing it to the UI.
pub(super) async fn prepare(s: &WebState, session: Session) -> Arc<Snapshot> {
    let original = Arc::new(Snapshot::new(session));
    let cancel = {
        let core = s.core.lock().await;
        match core.running.get(&original.id) {
            Some(run) if !run.closing => run.cancel.clone(),
            _ => return original,
        }
    };
    let input = original.clone();
    let checks = s.file_checks.clone();
    let timeout = Duration::from_secs(original.config.tool_timeout_secs);
    // A stopped run must not keep its slot and queued snapshots while waiting
    // for unrelated file work. Dropping run_queued also cancels a worker that
    // already started, while preserving the authoritative agent snapshot.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => original,
        result = s.file_io.run_queued(timeout, move |cancel| revalidated(input, &cancel, &checks)) => {
            result.unwrap_or(original)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryInput, MemoryKind, MemoryStatus};
    use tokio::sync::oneshot;

    fn workspace(dir: &FsPath) -> WebState {
        test_state(
            Config {
                projects: vec![Project {
                    root: dir.into(),
                    ..Default::default()
                }],
                ..Config::compact_test()
            },
            dir.join("config.toml"),
            Arc::new(OpenAiClient),
        )
        .unwrap()
    }

    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    /// Hold a real display calculation until the test releases it. The API
    /// request will wait for these byte counts on its file worker.
    async fn slow_counts(snapshot: Arc<Snapshot>) -> (Release, std::thread::JoinHandle<()>) {
        let (entered, started) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            snapshot.usage.get_or_init(|| {
                let _ = entered.send(());
                held.recv_timeout(Duration::from_secs(5)).unwrap();
                (snapshot.memory.bytes(), snapshot.history.bytes())
            });
        });
        let release = Release(Some(release));
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        (release, worker)
    }

    #[tokio::test]
    async fn a_slow_session_read_allows_other_sessions_and_preserves_later_edits() {
        let dir = tempfile::tempdir().unwrap();
        let state = workspace(dir.path());
        let (id, snapshot, revision) = {
            let mut core = state.core.lock().await;
            let id = core.order[0].clone();
            core.session_mut(&id).unwrap().history.push(
                vec![json!({"role":"assistant", "content":"retained history"})],
                true,
            );
            (id.clone(), core.sessions[&id].clone(), core.revision)
        };
        let (release, worker) = slow_counts(snapshot).await;
        let request = tokio::spawn(session_get(
            State(state.clone()),
            Path(id.clone()),
            Query(Page::default()),
        ));
        tokio::task::yield_now().await;
        let outcome = tokio::time::timeout(Duration::from_millis(500), async {
            let Json(created) = create_session(
                State(state.clone()),
                Json(NewSession {
                    workflow: default_workflow(),
                    project: Project {
                        root: dir.path().into(),
                        ..Default::default()
                    },
                }),
            )
            .await
            .unwrap();
            let other = created["id"].as_str().unwrap().to_string();
            let Json(display) = session_get(
                State(state.clone()),
                Path(other.clone()),
                Query(Page::default()),
            )
            .await
            .unwrap();
            assert_eq!(display["id"], other);
            let Json(list) = state_get(State(state.clone())).await;
            assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
            let _ = session_workflow(
                State(state.clone()),
                Path(id.clone()),
                Json(WorkflowSelection {
                    workflow: "answer".into(),
                }),
            )
            .await
            .unwrap();
            let _ = close_session(State(state.clone()), Path(other))
                .await
                .unwrap();
        })
        .await;
        let still_waiting = !request.is_finished();
        drop(release);
        worker.join().unwrap();
        outcome.expect("one slow session blocked other sessions or settings");
        assert!(still_waiting, "fixture did not hold the slow session read");
        let Json(old) = request.await.unwrap().unwrap();
        assert_eq!(old["revision"], revision);
        assert_eq!(old["workflow_mode"], "answer");
        let Json(latest) = session_get(State(state.clone()), Path(id), Query(Page::default()))
            .await
            .unwrap();
        assert_eq!(latest["workflow_mode"], "answer");
        assert!(latest["revision"].as_u64().unwrap() > revision);
    }

    #[tokio::test]
    async fn closing_a_session_during_a_slow_read_does_not_restore_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = workspace(dir.path());
        let (id, snapshot) = {
            let core = state.core.lock().await;
            let id = core.order[0].clone();
            (id.clone(), core.sessions[&id].clone())
        };
        let (release, worker) = slow_counts(snapshot).await;
        let request = tokio::spawn(session_get(
            State(state.clone()),
            Path(id.clone()),
            Query(Page::default()),
        ));
        tokio::task::yield_now().await;
        let closed = tokio::time::timeout(
            Duration::from_millis(500),
            close_session(State(state.clone()), Path(id)),
        )
        .await;
        drop(release);
        worker.join().unwrap();
        let _ = closed.expect("slow read blocked session closing").unwrap();
        assert_eq!(request.await.unwrap().unwrap_err().0, StatusCode::NOT_FOUND);
        assert!(state.core.lock().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn reads_revalidate_sources_and_refresh_cached_byte_counts_after_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn sample() {}\n").unwrap();
        let state = workspace(dir.path());
        let (id, memory_id) = {
            let mut core = state.core.lock().await;
            let id = core.order[0].clone();
            let session = core.session_mut(&id).unwrap();
            let read = tools::execute(session, "file_read", json!({"path":"sample.rs"})).unwrap();
            let sources = session
                .source_refs(&[read["source"]["id"].as_str().unwrap().to_string()])
                .unwrap();
            let memory = session
                .memory
                .save(
                    MemoryInput {
                        key: Some("sample".into()),
                        title: "sample".into(),
                        summary: "source evidence".into(),
                        body: "sample has no arguments".into(),
                        tags: vec![],
                        kind: MemoryKind::Fact,
                        inferred: false,
                        source_ids: vec![],
                        metadata: json!({}),
                        expected_revision: None,
                    },
                    sources,
                    &session.config,
                )
                .unwrap();
            (id, memory.id)
        };
        let Json(first) = session_get(
            State(state.clone()),
            Path(id.clone()),
            Query(Page::default()),
        )
        .await
        .unwrap();
        let Json(again) = session_get(
            State(state.clone()),
            Path(id.clone()),
            Query(Page::default()),
        )
        .await
        .unwrap();
        assert_eq!(first["usage"], again["usage"]);
        std::fs::write(&path, "fn sample(value: u64) {}\n").unwrap();
        let Json(memory) = memory_get(State(state.clone()), Path((id.clone(), memory_id.clone())))
            .await
            .unwrap();
        assert_eq!(memory["status"], "needs_review");
        let expected = {
            let mut core = state.core.lock().await;
            let session = core.session_mut(&id).unwrap();
            assert_eq!(
                session.memory.get(&memory_id).unwrap().status,
                MemoryStatus::NeedsReview
            );
            session
                .history
                .push(vec![json!({"role":"user", "content":"next request"})], true);
            (session.memory.bytes(), session.history.bytes())
        };
        let Json(updated) = session_get(State(state.clone()), Path(id), Query(Page::default()))
            .await
            .unwrap();
        assert_eq!(updated["usage"]["memory_bytes"], expected.0);
        assert_eq!(updated["usage"]["history_bytes"], expected.1);
        assert!(
            updated["usage"]["history_bytes"].as_u64().unwrap()
                > first["usage"]["history_bytes"].as_u64().unwrap()
        );
    }
}
