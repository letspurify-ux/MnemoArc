//! Keep blocking terminal writes off Tokio, including a pipe with no reader.
use std::{
    io::{self, Write},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum Target {
    Stdout,
    Stderr,
}

struct Chunk {
    target: Target,
    bytes: Vec<u8>,
}

struct StopOnDrop {
    cancel: CancellationToken,
    wake: mpsc::WeakSender<Chunk>,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.cancel.cancel();
        // A sender retained by an aborted producer must not leave an idle
        // writer waiting forever. WeakSender does not keep normal drains open.
        if let Some(sender) = self.wake.upgrade() {
            let _ = sender.try_send(Chunk {
                target: Target::Stdout,
                bytes: Vec::new(),
            });
        }
    }
}

#[derive(Clone)]
pub(crate) struct Sender(mpsc::Sender<Chunk>);

impl Sender {
    pub(crate) async fn write(
        &self,
        target: Target,
        text: &str,
        cancel: &CancellationToken,
    ) -> bool {
        let mut start = 0;
        while start < text.len() {
            let mut end = start.saturating_add(CHUNK_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            // Reserve before copying. A blocked terminal retains only the
            // chunk being written and one queued chunk, without snapshots.
            let permit = tokio::select! {
                biased;
                _ = cancel.cancelled() => return false,
                permit = self.0.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return false,
                },
            };
            permit.send(Chunk {
                target,
                bytes: text.as_bytes()[start..end].to_vec(),
            });
            start = end;
        }
        true
    }

    fn try_write(&self, target: Target, text: String) {
        let Ok(permit) = self.0.try_reserve() else {
            return;
        };
        let mut end = text.len().min(CHUNK_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        permit.send(Chunk {
            target,
            bytes: text.as_bytes()[..end].to_vec(),
        });
    }
}

pub(crate) struct Console {
    sender: Sender,
    finished: oneshot::Receiver<io::Result<()>>,
    cancel_on_drop: StopOnDrop,
}

impl Console {
    pub(crate) fn new() -> io::Result<Self> {
        // A stalled OS write can outlive its headless run. Count the actual
        // native writers across runs, as with the file/tool worker pools.
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self::with_writers(
            WORKERS.get_or_init(|| Arc::new(Semaphore::new(16))).clone(),
            io::stdout(),
            io::stderr(),
        )
    }

    fn with_writers(
        workers: Arc<Semaphore>,
        mut stdout: impl Write + Send + 'static,
        mut stderr: impl Write + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Chunk>(1);
        let cancel = CancellationToken::new();
        let cancel_on_drop = StopOnDrop {
            cancel: cancel.clone(),
            wake: sender.downgrade(),
        };
        let finished = crate::worker::spawn(
            workers,
            "mnemoarc-console",
            "console_worker_capacity: previous terminal writes are still running",
            move || {
                while let Some(chunk) = receiver.blocking_recv() {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let writer: &mut dyn Write = match chunk.target {
                        Target::Stdout => &mut stdout,
                        Target::Stderr => &mut stderr,
                    };
                    writer.write_all(&chunk.bytes)?;
                    writer.flush()?;
                }
                Ok(())
            },
        )?;
        Ok(Self {
            sender: Sender(sender),
            finished,
            cancel_on_drop,
        })
    }

    pub(crate) fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub(crate) async fn finish(self, report: &str, deadline: tokio::time::Instant) {
        let Self {
            sender,
            finished,
            cancel_on_drop: _cancel_on_drop,
        } = self;
        let _ = tokio::time::timeout_at(
            deadline,
            sender.write(Target::Stderr, report, &CancellationToken::new()),
        )
        .await;
        drop(sender);
        // Tokio shutdown never waits for the native writer. If its syscall
        // stays blocked, its permit and at most 128KiB remain owned there.
        let _ = tokio::time::timeout_at(deadline, finished).await;
    }
}

/// Diagnostics must never hold a model request or HTTP server inside a
/// blocking stdio call. One process-owned writer has a bounded queue; full or
/// failed output drops log lines while structured attempt records stay intact.
pub(crate) fn notice(target: Target, text: String) {
    static LOG: OnceLock<Option<Console>> = OnceLock::new();
    if let Some(console) = LOG.get_or_init(|| {
        Console::with_writers(Arc::new(Semaphore::new(1)), io::stdout(), io::stderr()).ok()
    }) {
        console.sender.try_write(target, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Utf8Writer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Utf8Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            // Windows consoles require valid UTF-8 on every individual write.
            std::str::from_utf8(bytes).map_err(io::Error::other)?;
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn large_unicode_output_drains_without_splitting_a_character() {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let console = Console::with_writers(
            Arc::new(Semaphore::new(1)),
            Utf8Writer(bytes.clone()),
            io::sink(),
        )
        .unwrap();
        let text = format!("x{}", "한글🦀".repeat(20_000));
        let sender = console.sender();
        assert!(
            sender
                .write(Target::Stdout, &text, &CancellationToken::new())
                .await
        );
        drop(sender);
        console
            .finish("", tokio::time::Instant::now() + Duration::from_secs(2))
            .await;
        assert_eq!(*bytes.lock().unwrap(), text.as_bytes());
    }

    #[tokio::test]
    async fn dropping_the_owner_releases_an_idle_writer_with_a_retained_sender() {
        let workers = Arc::new(Semaphore::new(1));
        let console = Console::with_writers(workers.clone(), io::sink(), io::sink()).unwrap();
        let sender = console.sender();
        drop(console);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if workers.available_permits() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the console left its idle native writer alive");
        assert!(
            !sender
                .write(Target::Stdout, "no owner", &CancellationToken::new())
                .await
        );
    }

    struct BlockedWriter {
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelled_output_keeps_its_slot_until_the_os_write_finishes() {
        let workers = Arc::new(Semaphore::new(1));
        let (entered, started) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let console = Console::with_writers(
            workers.clone(),
            BlockedWriter {
                entered: Some(entered),
                release: held,
            },
            io::sink(),
        )
        .unwrap();
        let sender = console.sender();
        assert!(
            sender
                .write(Target::Stdout, "first", &CancellationToken::new())
                .await
        );
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            sender
                .write(Target::Stdout, "queued", &CancellationToken::new())
                .await
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(!sender.write(Target::Stdout, "cancelled", &cancel).await);
        drop(sender);
        console
            .finish(
                "report",
                tokio::time::Instant::now() + Duration::from_millis(20),
            )
            .await;
        let occupied = workers.available_permits();
        let refused = Console::with_writers(workers.clone(), io::sink(), io::sink()).is_err();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if workers.available_permits() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(occupied, 0);
        assert!(
            refused,
            "a cancelled write granted another native writer slot"
        );
        let console = Console::with_writers(workers, io::sink(), io::sink()).unwrap();
        console
            .finish(
                "report",
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await;
    }
}
