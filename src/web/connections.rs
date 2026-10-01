//! Bound live HTTP connections and close them after the shutdown grace period.
use axum::serve::Listener;
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

pub(super) struct ManagedListener {
    inner: TcpListener,
    stopping: CancellationToken,
    connections: Arc<Semaphore>,
}

impl ManagedListener {
    pub(super) fn new(inner: TcpListener, stopping: CancellationToken) -> Self {
        // Empty headers and idle keep-alive sockets also own a connection task.
        // Count old tasks across server restarts until their sockets drop.
        static CONNECTIONS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        Self {
            inner,
            stopping,
            connections: CONNECTIONS
                .get_or_init(|| Arc::new(Semaphore::new(256)))
                .clone(),
        }
    }
}

impl Listener for ManagedListener {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Acquire before accept so excess peers wait in the bounded OS backlog
        // without allocating another server socket, router or request task.
        let permit = self
            .connections
            .clone()
            .acquire_owned()
            .await
            .expect("HTTP connection semaphore is never closed");
        let (inner, addr) = Listener::accept(&mut self.inner).await;
        let stopping = self.stopping.clone();
        let closing = Box::pin(async move {
            stopping.cancelled().await;
            // Let ordinary shutdown replies and stream terminators flush.
            // A peer that stops sending/reading cannot retain the socket,
            // request extractor and router state indefinitely.
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        (
            Connection {
                inner,
                closing,
                closed: false,
                _permit: permit,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(super) struct Connection {
    inner: TcpStream,
    closing: Pin<Box<dyn Future<Output = ()> + Send>>,
    closed: bool,
    _permit: OwnedSemaphorePermit,
}

impl Connection {
    fn check_shutdown(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.closed && self.closing.as_mut().poll(cx).is_ready() {
            self.closed = true;
        }
        if self.closed {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "server is shutting down",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check_shutdown(cx)?;
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_shutdown(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_shutdown(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_shutdown(cx)?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.check_shutdown(cx)?;
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn listener(connections: Arc<Semaphore>) -> ManagedListener {
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut listener = ManagedListener::new(inner, CancellationToken::new());
        listener.connections = connections;
        listener
    }

    #[tokio::test]
    async fn connection_capacity_counts_live_sockets_across_listener_restarts() {
        let connections = Arc::new(Semaphore::new(2));
        let mut first = listener(connections.clone()).await;
        let mut clients = Vec::new();
        let mut accepted = Vec::new();
        for _ in 0..2 {
            clients.push(
                TcpStream::connect(first.local_addr().unwrap())
                    .await
                    .unwrap(),
            );
            accepted.push(first.accept().await.0);
        }
        drop(first);
        let mut restarted = listener(connections.clone()).await;
        let mut waiting = TcpStream::connect(restarted.local_addr().unwrap())
            .await
            .unwrap();
        let excess = tokio::time::timeout(Duration::from_millis(50), restarted.accept()).await;
        assert!(
            excess.is_err(),
            "restart admitted sockets while old connections were retained"
        );
        drop(accepted.pop());
        let (mut admitted, _) = tokio::time::timeout(Duration::from_secs(2), restarted.accept())
            .await
            .unwrap();
        waiting.write_all(b"new").await.unwrap();
        let mut bytes = [0; 3];
        admitted.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"new");
        drop((admitted, accepted, clients, waiting, restarted));
        assert_eq!(connections.available_permits(), 2);
    }

    #[tokio::test]
    async fn aborting_a_pending_accept_releases_its_connection_reservation() {
        let connections = Arc::new(Semaphore::new(1));
        let mut listener = listener(connections.clone()).await;
        let accept = tokio::spawn(async move { listener.accept().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while connections.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        accept.abort();
        match accept.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted listener accepted a connection"),
        }
        assert_eq!(connections.available_permits(), 1);
    }
}
