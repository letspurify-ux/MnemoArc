//! System DNS can block after a request is cancelled. Count the real lookups
//! across clients and runtimes without making Tokio shutdown wait for them.
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    io,
    net::ToSocketAddrs,
    sync::{Arc, OnceLock},
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(super) struct SystemDns {
    workers: Arc<Semaphore>,
}

impl Default for SystemDns {
    fn default() -> Self {
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self {
            workers: WORKERS.get_or_init(|| Arc::new(Semaphore::new(16))).clone(),
        }
    }
}

impl SystemDns {
    fn resolve_with(
        &self,
        name: String,
        lookup: impl FnOnce(&str) -> io::Result<Addrs> + Send + 'static,
    ) -> Resolving {
        let workers = self.workers.clone();
        Box::pin(async move {
            let receiver = crate::worker::spawn(
                workers,
                "mnemoarc-dns",
                "dns_worker_capacity: previous system lookups are still running",
                move || lookup(&name),
            )?;
            receiver
                .await
                .map_err(|_| io::Error::other("dns_worker_panic: system lookup interrupted"))?
                .map_err(Into::into)
        })
    }
}

impl Resolve for SystemDns {
    fn resolve(&self, name: Name) -> Resolving {
        self.resolve_with(name.as_str().to_owned(), |name| {
            Ok(Box::new((name, 0).to_socket_addrs()?) as Addrs)
        })
    }
}
