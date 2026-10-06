//! Serialize in-process writes from validation through commit/rollback. Model
//! requests and reads stay concurrent; filesystem and database writes use
//! separate gates so a slow database cannot hold up document saves.
use anyhow::{Result, bail};
use std::sync::{
    Arc, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static FILE_WRITES: Mutex<()> = Mutex::new(());
static DATABASE_WRITES: Mutex<()> = Mutex::new(());

pub(super) struct WriteGuard<'a> {
    _lock: MutexGuard<'a, ()>,
    uncertain: Arc<AtomicBool>,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        // Publish the failure before the lock is released during unwinding.
        // Workers using the other gate must observe the same quarantine.
        if std::thread::panicking() {
            self.uncertain.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn external_write(name: &str) -> bool {
    matches!(
        name,
        "document_edit"
            | "document_edit_batch"
            | "file_edit"
            | "file_write"
            | "file_patch"
            | "db_execute"
    )
}

pub(crate) fn uncertain_write_error(error: &str) -> bool {
    matches!(
        super::recovery::error_code(error),
        "tool_worker_panic"
            | "tool_worker_unresolved"
            | "database_commit_uncertain"
            | "database_rollback_uncertain"
            | "file_patch_rollback_failed"
    )
}

pub(super) fn acquire(
    name: &str,
    cancel: &CancellationToken,
    uncertain: &Arc<AtomicBool>,
) -> Result<Option<WriteGuard<'static>>> {
    if !external_write(name) {
        return Ok(None);
    }
    let gate = if name == "db_execute" {
        &DATABASE_WRITES
    } else {
        &FILE_WRITES
    };
    let guard = acquire_gate(gate, cancel, uncertain)?;
    Ok(Some(WriteGuard {
        _lock: guard,
        uncertain: uncertain.clone(),
    }))
}

fn acquire_gate<'a>(
    gate: &'a Mutex<()>,
    cancel: &CancellationToken,
    uncertain: &AtomicBool,
) -> Result<MutexGuard<'a, ()>> {
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if uncertain.load(Ordering::Acquire) {
            bail!(
                "tool_worker_unresolved: previous external write outcome is unknown; inspect changes and restart before writing"
            );
        }
        match gate.try_lock() {
            Ok(guard) => {
                // Recheck after acquisition: another writer may have reported
                // an uncertain result while this worker was waiting.
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                if uncertain.load(Ordering::Acquire) {
                    bail!(
                        "tool_worker_unresolved: previous external write outcome is unknown; inspect changes and restart before writing"
                    );
                }
                return Ok(guard);
            }
            Err(TryLockError::Poisoned(_)) => {
                uncertain.store(true, Ordering::Release);
                bail!(
                    "tool_worker_unresolved: a writer panicked; inspect changes and restart before writing"
                );
            }
            // These are dedicated tool threads. A bounded wait lets their
            // cancellation/deadline return without executing a late write.
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Project},
        session::Session,
        tools,
    };
    use serde_json::json;

    #[test]
    fn cancelled_or_uncertain_waiter_never_acquires_the_gate() {
        for unknown in [false, true] {
            let gate = Mutex::new(());
            let held = gate.lock().unwrap();
            let cancel = CancellationToken::new();
            let uncertain = AtomicBool::new(false);
            std::thread::scope(|scope| {
                let waiter = scope.spawn(|| acquire_gate(&gate, &cancel, &uncertain).map(drop));
                if unknown {
                    uncertain.store(true, Ordering::Release);
                } else {
                    cancel.cancel();
                }
                let error = waiter.join().unwrap().unwrap_err().to_string();
                assert!(error.starts_with(if unknown {
                    "tool_worker_unresolved"
                } else {
                    "cancelled"
                }));
            });
            drop(held);
        }
    }

    #[test]
    fn a_waiting_write_validates_after_the_previous_writer_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "original").unwrap();
        let mut session = Session::new(
            Project {
                root: dir.path().into(),
                ..Default::default()
            },
            Config::compact_test(),
        );
        let held = FILE_WRITES.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                let result = tools::execute(&mut session, "file_write", json!({"path":"notes.txt", "content":"stale", "expected_hash":tools::hash(b"original")}));
                sent.send(()).unwrap();
                result
            });
            assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
            // Emulate the current gate owner's commit before releasing it.
            std::fs::write(&path, "previous writer").unwrap();
            drop(held);
            assert!(
                writer
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .starts_with("file_revision_conflict")
            );
        });
        assert_eq!(std::fs::read_to_string(path).unwrap(), "previous writer");
    }

    #[test]
    fn a_cancelled_waiting_tool_cannot_write_after_the_gate_opens() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Project {
                root: dir.path().into(),
                ..Default::default()
            },
            Config::compact_test(),
        );
        let uncertain = session.write_outcome_uncertain.clone();
        let cancel = CancellationToken::new();
        let held = FILE_WRITES.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let cancel = &cancel;
            scope.spawn(move || {
                let result = tools::execute_cancellable(
                    &mut session,
                    "file_write",
                    json!({"path":"cancelled.txt", "content":"must not be written"}),
                    cancel,
                );
                sent.send(result).unwrap();
            });
            assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
            cancel.cancel();
            let error = received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err();
            assert_eq!(error.to_string(), "cancelled");
            drop(held);
        });
        assert!(!dir.path().join("cancelled.txt").exists());
        assert!(!uncertain.load(Ordering::Acquire));
        // Cancellation must neither poison the gate nor block other sessions.
        let mut other = Session::new(
            Project {
                root: dir.path().into(),
                ..Default::default()
            },
            Config::compact_test(),
        );
        tools::execute(
            &mut other,
            "file_write",
            json!({"path":"other.txt", "content":"saved"}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("other.txt")).unwrap(),
            "saved"
        );
    }

    #[test]
    fn a_panicking_writer_quarantines_other_sessions_before_unlocking() {
        let gate = Mutex::new(());
        let uncertain = Arc::new(AtomicBool::new(false));
        let outcome = std::panic::catch_unwind(|| {
            let _write = WriteGuard {
                _lock: gate.lock().unwrap(),
                uncertain: uncertain.clone(),
            };
            panic!("simulated write failure");
        });
        assert!(outcome.is_err());
        assert!(uncertain.load(Ordering::Acquire));
        assert!(gate.is_poisoned());
    }
}
