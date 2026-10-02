use super::ApiError;
use anyhow::Result;
use axum::http::StatusCode;
use std::{
    io::Read,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

#[derive(Clone)]
pub(super) struct FileIo {
    workers: Arc<Semaphore>,
    stopping: CancellationToken,
}

impl FileIo {
    #[cfg(test)]
    pub(super) fn with_workers(workers: Arc<Semaphore>, stopping: CancellationToken) -> Self {
        Self { workers, stopping }
    }

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
        permit: Option<OwnedSemaphorePermit>,
        operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<(oneshot::Receiver<Result<T>>, DropGuard), ApiError> {
        if self.stopping.is_cancelled() {
            return Err(stopping());
        }
        let cancel = self.stopping.child_token();
        let cancel_on_drop = cancel.clone().drop_guard();
        let operation = move || {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            operation(cancel)
        };
        let receiver = match permit {
            Some(permit) => crate::worker::spawn_permitted(permit, "mnemoarc-web-io", operation),
            None => crate::worker::spawn(
                self.workers.clone(),
                "mnemoarc-web-io",
                "file_worker_capacity: 이전 파일 작업이 아직 실행 중입니다. 잠시 후 다시 시도하세요.",
                operation,
            ),
        }
        .map_err(|error| ApiError(StatusCode::SERVICE_UNAVAILABLE, error.to_string()))?;
        Ok((receiver, cancel_on_drop))
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        timeout: Duration,
        operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let (receiver, _cancel_on_drop) = self.start(None, operation)?;
        self.finish(receiver, timeout).await
    }

    /// Snapshot readers share existing snapshots while waiting. Queue short
    /// bursts without admitting extra live threads or rejecting normal polls.
    pub(super) async fn run_queued<T: Send + 'static>(
        &self,
        timeout: Duration,
        operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = tokio::select! {
            biased;
            _ = self.stopping.cancelled() => return Err(stopping()),
            _ = tokio::time::sleep_until(deadline) => return Err(timed_out()),
            result = self.workers.clone().acquire_owned() => result.map_err(|_| stopping())?,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(timed_out());
        }
        let (receiver, _cancel_on_drop) = self.start(Some(permit), operation)?;
        self.finish(
            receiver,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    }

    async fn finish<T>(
        &self,
        receiver: oneshot::Receiver<Result<T>>,
        timeout: Duration,
    ) -> Result<T, ApiError> {
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
        let (finished, cancel_on_drop) = self.start(None, move |cancel| {
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

    #[tokio::test]
    async fn queued_reads_wait_for_capacity_and_cancel_before_starting_work() {
        for outcome in ["release", "abort", "shutdown", "timeout"] {
            let io = isolated();
            let permit = io.workers.clone().acquire_owned().await.unwrap();
            let started = Arc::new(AtomicUsize::new(0));
            let count = started.clone();
            let reader = io.clone();
            let request = tokio::spawn(async move {
                reader
                    .run_queued(
                        Duration::from_millis(if outcome == "timeout" { 50 } else { 2000 }),
                        move |_| {
                            count.fetch_add(1, Ordering::SeqCst);
                            Ok(42)
                        },
                    )
                    .await
            });
            tokio::task::yield_now().await;
            assert!(
                !request.is_finished(),
                "queued read rejected a short worker burst"
            );
            assert_eq!(started.load(Ordering::SeqCst), 0);
            if outcome == "release" {
                drop(permit);
                assert_eq!(request.await.unwrap().unwrap(), 42);
                assert_eq!(started.load(Ordering::SeqCst), 1);
                continue;
            }
            match outcome {
                "abort" => {
                    request.abort();
                    assert!(request.await.unwrap_err().is_cancelled());
                }
                "shutdown" => {
                    io.stopping.cancel();
                    assert_eq!(
                        request.await.unwrap().unwrap_err().0,
                        StatusCode::SERVICE_UNAVAILABLE
                    );
                }
                _ => assert_eq!(
                    request.await.unwrap().unwrap_err().0,
                    StatusCode::GATEWAY_TIMEOUT
                ),
            }
            drop(permit);
            assert_eq!(
                started.load(Ordering::SeqCst),
                0,
                "abandoned read started a worker"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_queued_read_does_not_start_when_capacity_returns() {
        let io = isolated();
        let permit = io.workers.clone().acquire_owned().await.unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let reader = io.clone();
        let request = tokio::spawn(async move {
            reader
                .run_queued(Duration::from_secs(1), move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        tokio::task::yield_now().await;
        // Make both the deadline and permit ready before the waiter polls.
        tokio::time::advance(Duration::from_secs(2)).await;
        drop(permit);
        let result = request.await.unwrap();
        assert_eq!(result.unwrap_err().0, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "expired read started a file worker"
        );
    }

    struct FailedReader(bool);
    impl Read for FailedReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.0, "simulated file reader panic");
            Err(std::io::Error::other("simulated file read failure"))
        }
    }

    #[tokio::test]
    async fn download_read_failures_and_panics_are_errors() {
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
        }
    }
}
