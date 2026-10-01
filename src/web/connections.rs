//! Close stalled HTTP connections after the server's shutdown grace period.
use axum::serve::Listener;
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

pub(super) struct ManagedListener {
    inner: TcpListener,
    stopping: CancellationToken,
}

impl ManagedListener {
    pub(super) fn new(inner: TcpListener, stopping: CancellationToken) -> Self {
        Self { inner, stopping }
    }
}

impl Listener for ManagedListener {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
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
