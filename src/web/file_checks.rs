//! Short-lived display digests shared by sessions. Mutating tools continue to
//! hash real files; display reads check the file identity on every cache hit.
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio_util::sync::CancellationToken;

const CAPACITY: usize = 128;
const MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Version {
    fn read(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            bail!("not a regular file");
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}

enum Outcome {
    Pending,
    Ready(String, Instant),
    Abandoned,
}
struct Check {
    version: Version,
    outcome: Mutex<Outcome>,
    ready: Condvar,
}
#[derive(Default)]
struct Checks {
    entries: BTreeMap<PathBuf, (u64, Arc<Check>)>,
    sequence: u64,
}
#[derive(Clone, Default)]
pub(super) struct FileChecks(Arc<Mutex<Checks>>);

struct Claim {
    cache: FileChecks,
    path: PathBuf,
    check: Arc<Check>,
    completed: bool,
}
impl Drop for Claim {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut cache = self.cache.0.lock().unwrap();
        if cache
            .entries
            .get(&self.path)
            .is_some_and(|(_, check)| Arc::ptr_eq(check, &self.check))
        {
            cache.entries.remove(&self.path);
        }
        *self.check.outcome.lock().unwrap() = Outcome::Abandoned;
        self.check.ready.notify_all();
    }
}

impl FileChecks {
    pub(super) fn hash(&self, path: &Path, cancel: &CancellationToken) -> Result<String> {
        self.hash_with(path, cancel, || {
            crate::tools::hash_file_cancelled(path, cancel)
        })
    }

    fn hash_with(
        &self,
        path: &Path,
        cancel: &CancellationToken,
        mut hash: impl FnMut() -> Result<String>,
    ) -> Result<String> {
        for _ in 0..3 {
            if cancel.is_cancelled() {
                bail!("cancelled");
            }
            let version = Version::read(path)?;
            let (check, owner) = {
                let mut cache = self.0.lock().unwrap();
                cache.sequence = cache.sequence.saturating_add(1);
                let sequence = cache.sequence;
                let reusable = cache.entries.get_mut(path).filter(|(_, check)| {
                    check.version == version
                        && match &*check.outcome.lock().unwrap() {
                            Outcome::Pending => true,
                            Outcome::Ready(_, checked) => checked.elapsed() < MAX_AGE,
                            Outcome::Abandoned => false,
                        }
                });
                if let Some((used, check)) = reusable {
                    *used = sequence;
                    (check.clone(), false)
                } else {
                    if cache.entries.len() >= CAPACITY {
                        if let Some(oldest) = cache
                            .entries
                            .iter()
                            .min_by_key(|(_, (used, _))| *used)
                            .map(|(path, _)| path.clone())
                        {
                            cache.entries.remove(&oldest);
                        }
                    }
                    let check = Arc::new(Check {
                        version: version.clone(),
                        outcome: Mutex::new(Outcome::Pending),
                        ready: Condvar::new(),
                    });
                    cache.entries.insert(path.into(), (sequence, check.clone()));
                    (check, true)
                }
            };
            if owner {
                let mut claim = Claim {
                    cache: self.clone(),
                    path: path.into(),
                    check: check.clone(),
                    completed: false,
                };
                let digest = hash()?;
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                if Version::read(path)? != version {
                    continue;
                }
                *check.outcome.lock().unwrap() = Outcome::Ready(digest.clone(), Instant::now());
                claim.completed = true;
                check.ready.notify_all();
                return Ok(digest);
            }
            let mut outcome = check.outcome.lock().unwrap();
            let ready = loop {
                if cancel.is_cancelled() {
                    bail!("cancelled");
                }
                match &*outcome {
                    Outcome::Ready(digest, checked) => break Some((digest.clone(), *checked)),
                    Outcome::Abandoned => break None,
                    Outcome::Pending => {
                        // A cancelled reader releases its file-worker slot
                        // even when another reader is stuck in a kernel read.
                        outcome = check
                            .ready
                            .wait_timeout(outcome, Duration::from_millis(20))
                            .unwrap()
                            .0;
                    }
                }
            };
            drop(outcome);
            if let Some((digest, checked)) = ready {
                if checked.elapsed() < MAX_AGE && Version::read(path)? == version {
                    return Ok(digest);
                }
            }
        }
        bail!("file_changed: file changed during display validation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn concurrent_sessions_share_one_digest_and_edits_and_replacements_invalidate_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        std::fs::write(&path, "first").unwrap();
        let checks = FileChecks::default();
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..12 {
                let checks = &checks;
                let path = &path;
                let calls = &calls;
                scope.spawn(move || {
                    assert_eq!(
                        checks
                            .hash_with(path, &CancellationToken::new(), || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                crate::tools::hash_file(path)
                            })
                            .unwrap(),
                        crate::tools::hash(b"first")
                    );
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "other").unwrap();
        #[cfg(unix)]
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        #[cfg(not(unix))]
        let _ = modified;
        assert_eq!(
            checks.hash(&path, &CancellationToken::new()).unwrap(),
            crate::tools::hash(b"other")
        );
        let replacement = dir.path().join("replacement.rs");
        std::fs::write(&replacement, "third").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(
            checks.hash(&path, &CancellationToken::new()).unwrap(),
            crate::tools::hash(b"third")
        );
        std::fs::remove_file(&path).unwrap();
        assert!(checks.hash(&path, &CancellationToken::new()).is_err());
    }

    #[test]
    fn a_waiter_can_cancel_without_cancelling_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        std::fs::write(&path, "data").unwrap();
        let checks = FileChecks::default();
        let (entered, started) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let owner_checks = checks.clone();
            let owner_path = path.clone();
            let owner = scope.spawn(move || {
                owner_checks.hash_with(&owner_path, &CancellationToken::new(), || {
                    entered.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(3)).unwrap();
                    crate::tools::hash_file(&owner_path)
                })
            });
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            let cancel = CancellationToken::new();
            let (finished, done) = std::sync::mpsc::channel();
            let waiter_cancel = cancel.clone();
            scope.spawn(move || {
                finished
                    .send(checks.hash(&path, &waiter_cancel).is_err())
                    .unwrap();
            });
            cancel.cancel();
            let cancelled = done.recv_timeout(Duration::from_millis(300));
            release.send(()).unwrap();
            assert!(cancelled.unwrap());
            assert_eq!(owner.join().unwrap().unwrap(), crate::tools::hash(b"data"));
        });
    }

    #[test]
    fn cached_paths_remain_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let checks = FileChecks::default();
        for i in 0..CAPACITY + 20 {
            let path = dir.path().join(i.to_string());
            std::fs::write(&path, "data").unwrap();
            checks.hash(&path, &CancellationToken::new()).unwrap();
        }
        assert_eq!(checks.0.lock().unwrap().entries.len(), CAPACITY);
    }
}
