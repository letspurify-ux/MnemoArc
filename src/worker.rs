//! Count live blocking operations, including ones whose async receiver was dropped.
use std::sync::Arc;
use tokio::sync::{Semaphore, oneshot};

pub(crate) fn spawn<T: Send + 'static>(
    workers: Arc<Semaphore>,
    name: &str,
    capacity_error: &'static str,
    operation: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<oneshot::Receiver<T>> {
    let permit = workers
        .try_acquire_owned()
        .map_err(|_| std::io::Error::other(capacity_error))?;
    let (sender, receiver) = oneshot::channel();
    // Kernel calls cannot be aborted safely. Keep their slot until the thread
    // drops its inputs and undelivered result, without blocking Tokio shutdown.
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(operation());
        })?;
    Ok(receiver)
}
