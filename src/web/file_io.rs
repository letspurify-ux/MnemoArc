use super::ApiError;
use anyhow::Result;
use axum::http::StatusCode;
use std::{
    io::Read,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

#[derive(Clone)]
pub(super) struct FileIo {
    workers: Arc<Semaphore>,
    stopping: CancellationToken,
}

impl FileIo {
    pub(super) fn new(stopping: CancellationToken) -> Self {
        // A stuck operation can outlive a server restart. Count it across all
        // WebState instances, rather than granting another set of slots.
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self {
            workers: WORKERS.get_or_init(|| Arc::new(Semaphore::new(16))).clone(),
            stopping,
        }
    }

    fn start<T: Send + 'static>(
        &self,
        operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<(oneshot::Receiver<Result<T>>, DropGuard), ApiError> {
        if self.stopping.is_cancelled() {
            return Err(stopping());
        }
        let cancel = self.stopping.child_token();
        let cancel_on_drop = cancel.clone().drop_guard();
        let receiver = crate::worker::spawn(
            self.workers.clone(),
            "mnemoarc-web-io",
            "file_worker_capacity: 이전 파일 작업이 아직 실행 중입니다. 잠시 후 다시 시도하세요.",
            move || {
                if cancel.is_cancelled() {
                    anyhow::bail!("cancelled");
                }
                operation(cancel)
            },
        )
        .map_err(|error| ApiError(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        Ok((receiver, cancel_on_drop))
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        timeout: Duration,
        operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let (receiver, _cancel_on_drop) = self.start(operation)?;
        tokio::select! {
            biased;
            _ = self.stopping.cancelled() => Err(stopping()),
            result = receiver => result
                .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "file_worker_panic: 파일 작업이 중단되었습니다.".into()))?
                .map_err(ApiError::from),
            _ = tokio::time::sleep(timeout) => Err(timed_out()),
        }
    }

    pub(super) fn stream(
        self,
        mut reader: impl Read + Send + 'static,
        timeout: Duration,
    ) -> Result<impl futures_util::Stream<Item = std::io::Result<Vec<u8>>>, ApiError> {
        // One worker per download, with at most one queued chunk. Waiting for
        // queue space must observe cancellation and timeout even when Hyper
        // cannot poll the body because the client's socket is full.
        let (sender, receiver) = mpsc::channel(1);
        let runtime = tokio::runtime::Handle::current();
        let (finished, cancel_on_drop) = self.start(move |cancel| {
            loop {
                if cancel.is_cancelled() {
                    anyhow::bail!("cancelled");
                }
                let mut bytes = vec![0; 64 * 1024];
                let read = reader.read(&mut bytes)?;
                if read == 0 {
                    break;
                }
                bytes.truncate(read);
                let sent = runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => anyhow::bail!("cancelled"),
                        result = tokio::time::timeout(timeout, sender.send(bytes)) => {
                            result.map_err(|_| anyhow::anyhow!(timed_out().1))
                        }
                    }
                })?;
                if sent.is_err() {
                    break;
                }
            }
            Ok(())
        })?;
        let stopping_token = self.stopping;
        Ok(futures_util::stream::try_unfold(
            (receiver, finished, cancel_on_drop),
            move |(mut receiver, finished, cancel_on_drop)| {
                let stopping_token = stopping_token.clone();
                async move {
                    tokio::select! {
                        biased;
                        _ = stopping_token.cancelled() => Err(std::io::Error::other(stopping().1)),
                        result = tokio::time::timeout(timeout, async move {
                            if let Some(bytes) = receiver.recv().await {
                                Ok(Some((bytes, (receiver, finished, cancel_on_drop))))
                            } else {
                                finished.await
                                    .map_err(|_| std::io::Error::other("file_worker_panic: download interrupted"))?
                                    .map_err(std::io::Error::other)?;
                                Ok(None)
                            }
                        }) => result.map_err(|_| std::io::Error::other(timed_out().1))?,
                    }
                }
            },
        ))
    }
}

fn timed_out() -> ApiError {
    ApiError(
        StatusCode::GATEWAY_TIMEOUT,
        "file_operation_timeout: 파일 작업 제한 시간을 초과했습니다.".into(),
    )
}

fn stopping() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "앱을 종료하고 있습니다.".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn isolated() -> FileIo {
        FileIo {
            workers: Arc::new(Semaphore::new(1)),
            stopping: CancellationToken::new(),
        }
    }

    async fn idle(io: &FileIo) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while io.workers.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("file worker retained its slot after dropping its resources");
    }

    #[test]
    fn timed_out_file_work_does_not_hold_runtime_shutdown_open() {
        let io = isolated();
        let workers = io.workers.clone();
        let file = Arc::new(tempfile::tempfile().unwrap());
        let weak = Arc::downgrade(&file);
        let (entered, started) = std::sync::mpsc::channel();
        let (release, hold) = std::sync::mpsc::channel();
        let (finished, shutdown) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(io.run(Duration::from_millis(200), move |_| {
                entered.send(()).unwrap();
                let _ = hold.recv();
                Ok(file)
            }));
            let error = result.unwrap_err();
            drop(runtime);
            finished.send(error.0).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let status = shutdown.recv_timeout(Duration::from_secs(2));
        let occupied = workers.available_permits() == 0 && weak.upgrade().is_some();
        release.send(()).unwrap();
        thread.join().unwrap();
        assert_eq!(
            status.expect("runtime waited for abandoned file work"),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert!(
            occupied,
            "a timed-out operation must keep counting its live thread and file"
        );
        for _ in 0..200 {
            if workers.available_permits() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(workers.available_permits(), 1);
        assert!(
            weak.upgrade().is_none(),
            "undelivered file handle was retained"
        );
    }

    #[tokio::test]
    async fn aborted_and_stopping_file_requests_cancel_without_admitting_more_workers() {
        for shutdown in [false, true] {
            let io = isolated();
            let file = Arc::new(tempfile::tempfile().unwrap());
            let weak = Arc::downgrade(&file);
            let (entered, started) = oneshot::channel();
            let (release, hold) = std::sync::mpsc::channel();
            let worker = io.clone();
            let request = tokio::spawn(async move {
                worker
                    .run(Duration::from_secs(60), move |cancel| {
                        let _ = entered.send(cancel);
                        let _ = hold.recv();
                        Ok(file)
                    })
                    .await
            });
            let cancel = tokio::time::timeout(Duration::from_secs(2), started)
                .await
                .unwrap()
                .unwrap();
            if shutdown {
                io.stopping.cancel();
                assert_eq!(
                    request.await.unwrap().unwrap_err().0,
                    StatusCode::SERVICE_UNAVAILABLE
                );
            } else {
                request.abort();
                assert!(request.await.unwrap_err().is_cancelled());
            }
            assert!(
                cancel.is_cancelled(),
                "abandoned directory traversal was not cancelled"
            );
            // A new server still counts an old operation stuck inside the OS.
            let restarted = FileIo {
                workers: io.workers.clone(),
                stopping: CancellationToken::new(),
            };
            for _ in 0..32 {
                let error = restarted
                    .run(Duration::from_secs(1), |_| Ok(()))
                    .await
                    .unwrap_err();
                assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
                assert!(error.1.starts_with("file_worker_capacity:"));
            }
            assert!(weak.upgrade().is_some());
            release.send(()).unwrap();
            idle(&io).await;
            assert!(weak.upgrade().is_none());
            assert_eq!(
                restarted
                    .run(Duration::from_secs(1), |_| Ok(42))
                    .await
                    .unwrap(),
                42
            );
        }
    }

    struct ReaderProbe {
        file: std::fs::File,
        _retained: Arc<()>,
        reads: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
    }
    impl Read for ReaderProbe {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 1 {
                self.entered.notify_one();
            }
            self.file.read(bytes)
        }
    }

    #[tokio::test]
    async fn abandoning_a_backpressured_download_drops_the_reader_and_worker() {
        let io = isolated();
        let mut file = tempfile::tempfile().unwrap();
        use std::io::{Seek, Write};
        file.write_all(&vec![b'x'; 512 * 1024]).unwrap();
        file.rewind().unwrap();
        let retained = Arc::new(());
        let weak = Arc::downgrade(&retained);
        let reads = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let stream = io
            .clone()
            .stream(
                ReaderProbe {
                    file,
                    _retained: retained,
                    reads: reads.clone(),
                    entered: entered.clone(),
                },
                Duration::from_secs(60),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "slow downloads queued unbounded file chunks"
        );
        assert_eq!(io.workers.available_permits(), 0);
        drop(stream);
        idle(&io).await;
        assert!(
            weak.upgrade().is_none(),
            "download retained its reader after body disconnect"
        );
    }

    #[tokio::test]
    async fn shutdown_or_timeout_releases_an_unconsumed_download() {
        for shutdown in [false, true] {
            let io = isolated();
            let mut file = tempfile::tempfile().unwrap();
            use std::io::{Seek, Write};
            file.write_all(&vec![b'x'; 512 * 1024]).unwrap();
            file.rewind().unwrap();
            let retained = Arc::new(());
            let weak = Arc::downgrade(&retained);
            let entered = Arc::new(tokio::sync::Notify::new());
            let stream = io
                .clone()
                .stream(
                    ReaderProbe {
                        file,
                        _retained: retained,
                        reads: Arc::new(AtomicUsize::new(0)),
                        entered: entered.clone(),
                    },
                    if shutdown {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_millis(100)
                    },
                )
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            if shutdown {
                io.stopping.cancel();
            }
            // Hyper can stop polling a body while the socket is full. Resource
            // release must not require another body poll or its eventual drop.
            let settled = tokio::time::timeout(Duration::from_secs(2), async {
                while io.workers.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let released = weak.upgrade().is_none();
            drop(stream);
            idle(&io).await;
            settled.expect("unconsumed download retained its worker after shutdown or timeout");
            assert!(released, "unconsumed download retained its file reader");
        }
    }

    struct FailedReader(bool);
    impl Read for FailedReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.0, "simulated file reader panic");
            Err(std::io::Error::other("simulated file read failure"))
        }
    }

    #[tokio::test]
    async fn download_read_failures_and_panics_are_errors_and_release_the_worker() {
        let io = isolated();
        for panic in [false, true] {
            let stream = io
                .clone()
                .stream(FailedReader(panic), Duration::from_secs(2))
                .unwrap();
            tokio::pin!(stream);
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(error.to_string().contains(if panic {
                "file_worker_panic"
            } else {
                "simulated file read failure"
            }));
            assert!(stream.next().await.is_none());
            idle(&io).await;
        }
    }
}
