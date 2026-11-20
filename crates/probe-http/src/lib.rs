//! [`ProbeServer`] — the one hand-rolled HTTP probe/metrics server the runner
//! binaries (the b2bua worker, the front proxy, the route guard, the CDR
//! consumer) expose on their scrape port. A tiny tokio TCP handler (no HTTP
//! framework dep) serving a fixed route set, each route cheap and
//! non-blocking:
//!
//! - `GET /metrics` — Prometheus exposition, body supplied per-scrape by the
//!   injected [`ProbeRoutes::metrics`] closure (each runner concatenates its own
//!   metric sources: proxy text + txn/jemalloc for the worker, proxy text +
//!   jemalloc for the proxy).
//! - `GET /healthz` — liveness: `200 ok` while the process is up and, for a
//!   server started with a [`LiveFn`] ([`ProbeServer::start_live`]), while it
//!   reports itself live; `503 stalled` otherwise, so the k8s livenessProbe
//!   restarts a process that runs but no longer turns its loops.
//! - `GET /readyz` and its `GET /ready` alias — `200`/`503` driven by the
//!   injected [`ProbeRoutes::ready`] closure's 3-valued [`ProbeState`], so the
//!   k8s readinessProbe withholds traffic from a not-ready / draining pod.
//!   `503 stalled` / `503 not-ready` otherwise; `404` for a server given none.
//! - `GET /fitz` — `200 fit` / `503 unfit <reason>` from the injected
//!   [`ProbeRoutes::fit`] closure: whether the process could carry traffic,
//!   its drain latch aside, read by a local guard that polls it far more
//!   often than a kubelet probe; `404` for a server given none.
//! - everything else — `404`. The operator routes under `/admin/` and the
//!   debug routes under `/debug/` (`debug.rs`), which cost CPU or memory
//!   while they run, are served by an [`AdminServer`] on a loopback listener
//!   of its own.
//!
//! [`parse_socket_addr`] is the address grammar of the proxy's and the route
//! guard's HTTP listener variables and of `B2BUA_ADMIN`; the worker kit reads
//! its other listeners with it before resolving a host name.
//!
//! ## Why one module
//! The servers differ only in the `/metrics` body and the readiness predicate —
//! exactly the closures of [`ProbeRoutes`]. Everything else (the accept loop of
//! `accept.rs`, the exact-path match, the response framing, the **fully-idle
//! connection cut**) is shared transport, so the hardening applies to each by
//! construction.
//!
//! ## Fully-idle connection cut (auto resource exclusion, not DDoS)
//! The scrape port is internal to the cluster, so this is not DDoS
//! defence — it is reclamation of file descriptors stranded by an *involuntary*
//! bug: a client (a wedged scraper, a half-open probe, a port check) that
//! connects and then never sends its request line would otherwise park a task +
//! fd forever, and enough of them starve the SIP socket. A connection that is
//! **fully idle** — zero bytes received within [`IDLE_TIMEOUT`] — is dropped.
//! The window is deliberately long: a healthy probe/scrape sends its request
//! immediately and never approaches it; only a genuinely stuck connection does.
//! Once the request line has been read the timeout no longer applies, so a long
//! flamegraph capture on an [`AdminServer`], which reads its requests the same
//! way, is unaffected.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

mod accept;
mod addr;
mod admin;
mod admin_server;
mod debug;
mod request;

use accept::Incoming;
pub use addr::parse_socket_addr;
pub use admin::{AdminFn, AdminReply};
pub use admin_server::{AdminRoutes, AdminServer};
pub use debug::HeapDumpFn;

/// The readiness a runner reports for `GET /readyz` (and the `/ready` alias).
/// Three-valued so the HTTP probe can distinguish a node still coming up from
/// one shutting down — matching the OPTIONS self-report, which carries the same
/// distinction (`text="not-ready"` vs `text="draining"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
    /// Fit to take traffic. `200 ready`.
    Ready,
    /// Coming up / not yet caught up. `503 not-ready`.
    NotReady,
    /// Shutting down (SIGTERM). `503 draining`. k8s unpublishes the pod.
    Draining,
}

/// What a runner reports for `GET /fitz`: fit to carry traffic, or unfit
/// for the reason it names (one lowercase token, no space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fitness {
    /// `200 fit`.
    Fit,
    /// `503 unfit <reason>`.
    Unfit(&'static str),
}

/// Fitness provider for `/fitz` — cheap + non-blocking, called per poll.
pub type FitFn = Arc<dyn Fn() -> Fitness + Send + Sync>;

/// Per-scrape `/metrics` body provider. Each runner builds its own (the lib
/// can't depend on the runner's metric sources — e.g. jemalloc `mallctl` would
/// pull a C jemalloc into every build/test).
pub type MetricsFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Readiness provider for `/readyz` — cheap + non-blocking, called per probe.
pub type ReadyFn = Arc<dyn Fn() -> ProbeState + Send + Sync>;

/// Liveness provider for `/healthz` — cheap + non-blocking, called per probe:
/// `false` when the process runs but no longer serves.
pub type LiveFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// The things that vary between the worker's and the proxy's probe server.
/// Everything else is shared transport in [`ProbeServer`].
#[derive(Clone)]
pub struct ProbeRoutes {
    /// `GET /metrics` body, regenerated per scrape.
    pub metrics: MetricsFn,
    /// `GET /readyz` / `GET /ready` state.
    pub ready: ReadyFn,
    /// `GET /fitz` fitness; `None` ⇒ `404`.
    pub fit: Option<FitFn>,
}

/// A connection that sends nothing for this long is cut and its fd freed.
/// See the module docs: this is bug-stranded-fd reclamation, not DDoS defence,
/// so it is long enough that a healthy probe/scrape never nears it.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A running probe server. Aborts its accept loop on drop, so the binding is
/// released when the handle is dropped at process exit.
pub struct ProbeServer {
    addr: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl ProbeServer {
    /// Bind on `addr` (port 0 for an ephemeral test port) and spawn the accept
    /// loop. The bound address is available via [`ProbeServer::addr`].
    pub async fn start(addr: std::net::SocketAddr, routes: ProbeRoutes) -> std::io::Result<Self> {
        Self::start_with(addr, routes, IDLE_TIMEOUT).await
    }

    /// Like [`start`](Self::start), with `/healthz` answering `503` while
    /// `live` reports `false`.
    pub async fn start_live(
        addr: std::net::SocketAddr,
        routes: ProbeRoutes,
        live: LiveFn,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Self::serve(listener, Served { routes, live: Some(live) }, IDLE_TIMEOUT)
    }

    /// Like [`start`](Self::start) but with an explicit fully-idle cut window —
    /// tests pass a short one to exercise the cut without waiting [`IDLE_TIMEOUT`].
    pub async fn start_with(
        addr: std::net::SocketAddr,
        routes: ProbeRoutes,
        idle_timeout: Duration,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Self::serve_on_with(listener, routes, idle_timeout)
    }

    /// Serve on a pre-bound listener. The caller owns bind policy — ordering
    /// relative to its other sockets, EADDRINUSE retry — and can hold the bound
    /// port long before route construction; this only runs the accept loop.
    pub fn serve_on(listener: TcpListener, routes: ProbeRoutes) -> std::io::Result<Self> {
        Self::serve_on_with(listener, routes, IDLE_TIMEOUT)
    }

    /// [`serve_on`](Self::serve_on), with `/healthz` answering `503` while
    /// `live` reports `false` ([`start_live`](Self::start_live)).
    pub fn serve_on_live(
        listener: TcpListener,
        routes: ProbeRoutes,
        live: LiveFn,
    ) -> std::io::Result<Self> {
        Self::serve(listener, Served { routes, live: Some(live) }, IDLE_TIMEOUT)
    }

    /// [`serve_on`](Self::serve_on) with an explicit fully-idle cut window.
    pub fn serve_on_with(
        listener: TcpListener,
        routes: ProbeRoutes,
        idle_timeout: Duration,
    ) -> std::io::Result<Self> {
        Self::serve(listener, Served { routes, live: None }, idle_timeout)
    }

    fn serve(
        listener: impl Incoming,
        routes: Served,
        idle_timeout: Duration,
    ) -> std::io::Result<Self> {
        let local = listener.local_addr()?;
        let task = tokio::spawn(accept::run(listener, move |stream| {
            let served = routes.clone();
            async move {
                let _ = handle_conn(stream, served, idle_timeout).await;
            }
        }));
        Ok(Self { addr: local, task })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

/// The routes one server answers: the runner's, and its liveness if any.
#[derive(Clone)]
pub(crate) struct Served {
    routes: ProbeRoutes,
    live: Option<LiveFn>,
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn handle_conn(
    mut stream: TcpStream,
    served: Served,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let Served { routes, live } = served;
    let Some(request) = request::read(&mut stream, idle_timeout).await? else {
        return Ok(()); // silent client — drop
    };
    let (status, content_type, body) = match request.path.as_str() {
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", (routes.metrics)()),
        "/healthz" => match live.as_ref().is_none_or(|live| live()) {
            true => ("200 OK", "text/plain", "ok\n".to_string()),
            false => ("503 Service Unavailable", "text/plain", "stalled\n".to_string()),
        },
        // `/ready` (worker) and `/readyz` (proxy) are the same probe.
        "/readyz" | "/ready" => match (routes.ready)() {
            ProbeState::Ready => ("200 OK", "text/plain", "ready\n".to_string()),
            ProbeState::NotReady => {
                ("503 Service Unavailable", "text/plain", "not-ready\n".to_string())
            }
            ProbeState::Draining => {
                ("503 Service Unavailable", "text/plain", "draining\n".to_string())
            }
        },
        "/fitz" => match routes.fit.as_ref().map(|fit| fit()) {
            Some(Fitness::Fit) => ("200 OK", "text/plain", "fit\n".to_string()),
            Some(Fitness::Unfit(reason)) => {
                ("503 Service Unavailable", "text/plain", format!("unfit {reason}\n"))
            }
            None => ("404 Not Found", "text/plain", "not found\n".to_string()),
        },
        _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
