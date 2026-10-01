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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::SocketAddr, time::Duration};

    fn addresses() -> Addrs {
        Box::new(std::iter::once(SocketAddr::from(([127, 0, 0, 1], 0))))
    }

    #[tokio::test]
    async fn cancelled_dns_keeps_its_slot_until_the_system_lookup_finishes() {
        let workers = Arc::new(Semaphore::new(1));
        let resolver = SystemDns {
            workers: workers.clone(),
        };
        let (entered, started) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let lookup = tokio::spawn(resolver.resolve_with("held.invalid".into(), move |_| {
            entered.send(()).unwrap();
            held.recv().unwrap();
            Ok(addresses())
        }));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if started.try_recv().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        lookup.abort();
        assert!(lookup.await.is_err_and(|error| error.is_cancelled()));
        let occupied = workers.available_permits();
        let refused = resolver
            .resolve_with("next.invalid".into(), |_| Ok(addresses()))
            .await;
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
            refused.is_err(),
            "cancellation admitted another live DNS lookup"
        );
        assert!(
            resolver
                .resolve_with("recovered.invalid".into(), |_| Ok(addresses()))
                .await
                .is_ok()
        );
    }

    #[test]
    fn runtime_shutdown_does_not_wait_for_abandoned_system_dns() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let (finished, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let resolver = SystemDns {
                    workers: Arc::new(Semaphore::new(1)),
                };
                let lookup = resolver.resolve_with("held.invalid".into(), move |_| {
                    entered.send(()).unwrap();
                    held.recv().unwrap();
                    Ok(addresses())
                });
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), lookup)
                        .await
                        .is_err()
                );
            });
            drop(runtime);
            finished.send(()).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let shutdown = stopped.recv_timeout(Duration::from_secs(1));
        // Unblock the simulated OS resolver even when the assertion regresses.
        release.send(()).unwrap();
        thread.join().unwrap();
        shutdown.expect("cancelled DNS kept Tokio runtime shutdown waiting for the OS resolver");
    }
}
