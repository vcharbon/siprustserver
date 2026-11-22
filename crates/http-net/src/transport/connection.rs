//! The real client's connections. [`Connector`] dials each one with the
//! client's TCP settings and wraps its stream in a [`TrackedStream`], whose
//! [`Tracked`] record rides hyper-util's `Connected::extra`, so a request
//! reads back the record of the connection it took.
//!
//! A connection's end is counted once, by why the client closed it
//! ([`CloseCause`]): an answer's verdict and an abandoned request are
//! counted where they happen; a connection the peer ended, or that a failed
//! exchange or a refused body ended, is not counted; any other is closed by
//! its pool, [`CloseCause::Rotation`] once its client retired, else
//! [`CloseCause::Idle`] (a client that keeps no pool counts none). A
//! connection a request abandoned is closed by a reset (`SO_LINGER` 0, no
//! FIN first), so it leaves no socket in `TIME_WAIT`.

use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use super::target_resolve::TargetResolve;
use crate::ejection::{self, CloseCause, ConnectionRecord};
use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;

/// Whether a client still takes requests, and whether it pools: what a
/// connection's uncounted end is counted as.
pub(super) struct ClientLife {
    retired: AtomicBool,
    pools: bool,
}

impl ClientLife {
    /// A client taking requests, keeping a pool when `pools`.
    pub(super) fn new(pools: bool) -> Self {
        Self { retired: AtomicBool::new(false), pools }
    }

    /// The client takes no more requests.
    pub(super) fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    fn unmarked_close(&self) -> Option<CloseCause> {
        if self.retired.load(Ordering::Relaxed) {
            Some(CloseCause::Rotation)
        } else {
            self.pools.then_some(CloseCause::Idle)
        }
    }
}

/// The record of one connection.
pub(super) struct Tracked {
    /// What the ejection rules keep about it.
    pub(super) record: ConnectionRecord,
    /// Its peer, as the client's counters name it.
    peer: String,
    life: Arc<ClientLife>,
    /// Its end is counted, or is not to be.
    settled: AtomicBool,
    /// It closes by a reset.
    reset: AtomicBool,
}

impl Tracked {
    /// An answer's verdict closed it, and counted it.
    pub(super) fn closed_by_answer(&self) {
        self.settled.store(true, Ordering::Relaxed);
    }

    /// A request abandoned it: it closes by a reset. `true` the first time
    /// its end is settled so, for the caller to count.
    pub(super) fn abandoned(&self) -> bool {
        self.reset.store(true, Ordering::Relaxed);
        !self.settled.swap(true, Ordering::Relaxed)
    }

    /// An exchange on it failed, or the peer ended it: its end is no close
    /// of the client's.
    pub(super) fn failed(&self) {
        self.settled.store(true, Ordering::Relaxed);
    }
}

/// [`Tracked`] as `Connected::extra` carries it.
#[derive(Clone)]
pub(super) struct Tag(pub(super) Arc<Tracked>);

/// A client connection's stream and its record.
pub(super) struct TrackedStream {
    stream: TcpStream,
    tracked: Arc<Tracked>,
}

impl AsyncRead for TrackedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let (filled, room) = (buf.filled().len(), buf.remaining());
        let read = Pin::new(&mut self.stream).poll_read(cx, buf);
        let ended = match &read {
            Poll::Ready(Ok(())) => room > 0 && buf.filled().len() == filled,
            Poll::Ready(Err(_)) => true,
            Poll::Pending => false,
        };
        if ended {
            self.tracked.failed();
        }
        read
    }
}

impl AsyncWrite for TrackedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.stream).poll_write(cx, buf);
        if matches!(written, Poll::Ready(Err(_))) {
            self.tracked.failed();
        }
        written
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        if matches!(written, Poll::Ready(Err(_))) {
            self.tracked.failed();
        }
        written
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.tracked.reset.load(Ordering::Relaxed) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl Connection for TrackedStream {
    fn connected(&self) -> Connected {
        Connected::new().extra(Tag(self.tracked.clone()))
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        let tracked = &self.tracked;
        if tracked.reset.load(Ordering::Relaxed) {
            let _ = socket2::SockRef::from(&self.stream).set_linger(Some(Duration::ZERO));
        }
        if !tracked.settled.load(Ordering::Relaxed) {
            if let Some(cause) = tracked.life.unmarked_close() {
                ejection::closed(&tracked.peer, cause);
            }
        }
    }
}

/// The client's connector: a TCP connect under the client's settings, its
/// names resolved through its target, each connection tracked.
#[derive(Clone)]
pub(super) struct Connector {
    http: HttpConnector<TargetResolve>,
    /// The peer every connection counts under; `None`: the address dialled.
    peer: Option<Arc<str>>,
    life: Arc<ClientLife>,
}

impl Connector {
    /// A connector under `settings`, resolving through `target` when given,
    /// its connections counted under `peer` when given, for a client whose
    /// life `life` states.
    pub(super) fn new(
        settings: &ClientSettings,
        target: Option<Arc<ResolvedTarget>>,
        peer: Option<&str>,
        life: Arc<ClientLife>,
    ) -> Self {
        let mut http = HttpConnector::new_with_resolver(TargetResolve(target));
        http.set_connect_timeout(Some(settings.connect_timeout));
        http.set_nodelay(true);
        http.set_keepalive(Some(settings.keepalive_idle));
        http.set_keepalive_interval(Some(settings.keepalive_interval));
        http.set_keepalive_retries(Some(settings.keepalive_retries));
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        http.set_tcp_user_timeout(Some(settings.user_timeout));
        Self { http, peer: peer.map(Arc::from), life }
    }
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<TrackedStream>;
    type Error = Box<dyn Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let peer = match &self.peer {
            Some(peer) => peer.to_string(),
            None => dst.authority().map(|a| a.as_str().to_string()).unwrap_or_default(),
        };
        let life = self.life.clone();
        let connecting = self.http.call(dst);
        Box::pin(async move {
            let stream = connecting.await?.into_inner();
            let tracked = Arc::new(Tracked {
                record: ConnectionRecord::default(),
                peer,
                life,
                settled: AtomicBool::new(false),
                reset: AtomicBool::new(false),
            });
            Ok(TokioIo::new(TrackedStream { stream, tracked }))
        })
    }
}
