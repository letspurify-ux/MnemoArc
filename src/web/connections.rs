//! Bound HTTP connection lifetimes, capacity and shutdown.
use axum::{Router, serve::Listener};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
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
    task::JoinSet,
    time::Sleep,
};
use tokio_util::sync::CancellationToken;

const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

struct ConnectionTasks {
    tasks: JoinSet<()>,
    stopping: CancellationToken,
}

impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        self.stopping.cancel();
        // Let SSE terminators and ordinary responses flush even if the server
        // future was aborted. Each task enforces the same bounded grace period.
        self.tasks.detach_all();
    }
}

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

    pub(super) async fn serve(mut self, app: Router) {
        let stopping = self.stopping.clone();
        let mut connections = ConnectionTasks {
            tasks: JoinSet::new(),
            stopping: stopping.clone(),
        };
        loop {
            tokio::select! {
                biased;
                _ = stopping.cancelled() => break,
                _ = connections.tasks.join_next(), if !connections.tasks.is_empty() => {},
                (io, _) = self.accept() => {
                    let service = TowerToHyperService::new(app.clone());
                    let stopping = self.stopping.clone();
                    connections.tasks.spawn(async move {
                        // Axum's simple serve helper does not install a header
                        // timer. Body middleware never sees empty/partial headers.
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT);
                        let connection = builder.serve_connection(TokioIo::new(io), service);
                        tokio::pin!(connection);
                        tokio::select! {
                            biased;
                            _ = stopping.cancelled() => {
                                connection.as_mut().graceful_shutdown();
                                let _ = tokio::time::timeout(SHUTDOWN_GRACE, connection).await;
                            }
                            _ = &mut connection => {},
                        }
                    });
                }
            }
        }
        while connections.tasks.join_next().await.is_some() {}
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
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        });
        (
            Connection {
                inner,
                closing,
                closed: false,
                write_stall: None,
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
    write_stall: Option<Pin<Box<Sleep>>>,
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

    fn poll_write_result<T>(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if result.is_ready() {
            self.write_stall = None;
            return result;
        }
        // A file worker can time out while Hyper is waiting for socket space.
        // Wake the connection itself so its body, router and permit drop too.
        let stalled = self
            .write_stall
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(WRITE_TIMEOUT)));
        if stalled.as_mut().poll(cx).is_ready() {
            self.closed = true;
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "response write timed out",
            )))
        } else {
            Poll::Pending
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
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.poll_write_result(cx, result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_shutdown(cx)?;
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.poll_write_result(cx, result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_shutdown(cx)?;
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.poll_write_result(cx, result)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.check_shutdown(cx)?;
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.poll_write_result(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn unfinished_headers_release_the_connection_without_server_shutdown() {
        for (keep_alive, partial) in [(false, false), (false, true), (true, false), (true, true)] {
            let connections = Arc::new(Semaphore::new(2));
            let managed = listener(connections.clone()).await;
            let addr = managed.local_addr().unwrap();
            let app = axum::Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
            let server = tokio::spawn(managed.serve(app));
            let mut stalled = TcpStream::connect(addr).await.unwrap();
            if keep_alive {
                stalled
                    .write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
                read_headers(&mut stalled).await;
                let mut body = [0; 2];
                stalled.read_exact(&mut body).await.unwrap();
                assert_eq!(&body, b"ok");
            }
            if partial {
                stalled
                    .write_all(b"GET /ok HTTP/1.1\r\nHost:")
                    .await
                    .unwrap();
            }
            // Let Hyper poll the socket and register its header deadline.
            tokio::time::sleep(Duration::from_millis(50)).await;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::time::resume();
            let mut response = Vec::new();
            let closed = tokio::time::timeout(
                Duration::from_millis(300),
                stalled.read_to_end(&mut response),
            )
            .await;
            let released = tokio::time::timeout(Duration::from_secs(2), async {
                while connections.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            drop(stalled);
            server.abort();
            let _ = server.await;
            let _ = closed.expect("unfinished headers retained a socket and connection slot");
            released.expect("expired headers retained their connection permit");
        }
    }

    #[tokio::test]
    async fn an_unread_response_releases_its_body_and_connection_without_shutdown() {
        let connections = Arc::new(Semaphore::new(2));
        let managed = listener(connections.clone()).await;
        let addr = managed.local_addr().unwrap();
        let dropped = CancellationToken::new();
        let app = axum::Router::new().route(
            "/large",
            axum::routing::get({
                let dropped = dropped.clone();
                move || {
                    let guard = dropped.clone().drop_guard();
                    async move {
                        let stream = futures_util::stream::unfold(guard, |guard| async move {
                            Some((
                                Ok::<_, std::convert::Infallible>(vec![b'x'; 64 * 1024]),
                                guard,
                            ))
                        });
                        axum::body::Body::from_stream(stream)
                    }
                }
            }),
        );
        let server = tokio::spawn(managed.serve(app));
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let mut stalled = socket.connect(addr).await.unwrap();
        stalled
            .write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        read_headers(&mut stalled).await;
        // Stop reading while the server fills the socket's send buffers.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        let released = tokio::time::timeout(Duration::from_secs(2), async {
            dropped.cancelled().await;
            while connections.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        drop(stalled);
        server.abort();
        let _ = server.await;
        released.expect("an unread response retained its body and connection task");
    }

    #[tokio::test]
    async fn a_response_stream_can_wait_longer_than_the_socket_timeouts() {
        let managed = listener(Arc::new(Semaphore::new(2))).await;
        let addr = managed.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(|| async {
                let stream = futures_util::stream::unfold(0, |index| async move {
                    match index {
                        0 => Some((Ok::<_, std::convert::Infallible>("first"), 1)),
                        1 => {
                            tokio::time::sleep(Duration::from_secs(40)).await;
                            Some((Ok("second"), 2))
                        }
                        _ => None,
                    }
                });
                axum::body::Body::from_stream(stream)
            }),
        );
        let server = tokio::spawn(managed.serve(app));
        let mut response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/stream"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "first");
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(41)).await;
        tokio::time::resume();
        let remaining = tokio::time::timeout(Duration::from_secs(2), response.text()).await;
        server.abort();
        let _ = server.await;
        assert_eq!(remaining.unwrap().unwrap(), "second");
    }

    #[tokio::test]
    async fn stopping_or_aborting_serving_drops_handlers_and_all_connection_permits() {
        for abort in [false, true] {
            let connections = Arc::new(Semaphore::new(2));
            let managed = listener(connections.clone()).await;
            let addr = managed.local_addr().unwrap();
            let stopping = managed.stopping.clone();
            let entered = Arc::new(tokio::sync::Notify::new());
            let dropped = CancellationToken::new();
            let app = axum::Router::new().route(
                "/wait",
                axum::routing::get({
                    let entered = entered.clone();
                    let dropped = dropped.clone();
                    move || {
                        let entered = entered.clone();
                        let guard = dropped.clone().drop_guard();
                        async move {
                            let _guard = guard;
                            entered.notify_one();
                            std::future::pending::<&str>().await
                        }
                    }
                }),
            );
            let mut server = tokio::spawn(managed.serve(app));
            let mut client = TcpStream::connect(addr).await.unwrap();
            client
                .write_all(b"GET /wait HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            if abort {
                server.abort();
            } else {
                stopping.cancel();
            }
            let stopped = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
            let released = tokio::time::timeout(Duration::from_secs(2), async {
                dropped.cancelled().await;
                while connections.available_permits() != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            // Clean up even if a regression keeps the handler alive.
            server.abort();
            drop(client);
            let result =
                stopped.expect("server retained a handler that was not polling its socket");
            if abort {
                assert!(result.unwrap_err().is_cancelled());
            } else {
                result.unwrap();
            }
            released.expect("server retained a handler or connection permit after stopping");
        }
    }

    async fn read_headers(socket: &mut TcpStream) {
        let mut headers = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(socket.read_u8().await.unwrap());
                assert!(headers.len() < 4096);
            }
        })
        .await
        .unwrap();
        assert!(headers.starts_with(b"HTTP/1.1 200"));
    }

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
