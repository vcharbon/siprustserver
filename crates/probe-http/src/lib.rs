//! [`ProbeServer`] — the one hand-rolled HTTP probe/metrics server both runner
//! binaries (the b2bua worker and the front proxy) expose on their
//! observability port. A tiny tokio TCP handler (no HTTP framework dep) serving
//! a fixed route set:
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
//! - `GET /vrrp` — the check of a node that holds a virtual address: `200 ok`
//!   while `/healthz` passes and the injected [`ProbeRoutes::vrrp`] holds,
//!   `503 stalled` / `503 not-ready` otherwise; `404` for a server given none.
//! - any method on `/admin/…` — the runner's operator routes
//!   ([`ProbeRoutes::admin`]); `404` for a server given none. They sit on the
//!   observability port only, never on a SIP face.
//! - `GET /debug/flamegraph[?seconds=N]` — on-demand pprof CPU flamegraph SVG
//!   (internal debug route, NOT the SIP datapath; blocks for the sample window
//!   on a blocking thread).
//! - everything else — `404`.
//!
//! ## Why one module
//! The two servers differ only in the `/metrics` body and the readiness
//! predicate — exactly the two closures of [`ProbeRoutes`]. Everything else (the
//! accept loop, the exact-path match, the response framing, the **fully-idle
//! connection cut**, the flamegraph route) is shared transport, so the hardening
//! applies to both by construction.
//!
//! ## Fully-idle connection cut (auto resource exclusion, not DDoS)
//! The observability port is internal to the cluster, so this is not DDoS
//! defence — it is reclamation of file descriptors stranded by an *involuntary*
//! bug: a client (a wedged scraper, a half-open probe, a port check) that
//! connects and then never sends its request line would otherwise park a task +
//! fd forever, and enough of them starve the SIP socket. A connection that is
//! **fully idle** — zero bytes received within [`IDLE_TIMEOUT`] — is dropped.
//! The window is deliberately long: a healthy probe/scrape sends its request
//! immediately and never approaches it; only a genuinely stuck connection does.
//! Once the request line has been read the timeout no longer applies, so a long
//! flamegraph capture is unaffected.

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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

mod accept;
mod admin;

use accept::Incoming;
pub use admin::{AdminFn, AdminReply};

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

/// Per-scrape `/metrics` body provider. Each runner builds its own (the lib
/// can't depend on the runner's metric sources — e.g. jemalloc `mallctl` would
/// pull a C jemalloc into every build/test).
pub type MetricsFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Readiness provider for `/readyz` — cheap + non-blocking, called per probe.
pub type ReadyFn = Arc<dyn Fn() -> ProbeState + Send + Sync>;

/// Liveness provider for `/healthz` — cheap + non-blocking, called per probe:
/// `false` when the process runs but no longer serves.
pub type LiveFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// On-demand heap-profile dump for `GET /debug/heap` (jemalloc `prof.dump` →
/// jeprof/pprof bytes). Injected by the runner so the lib stays allocator-
/// agnostic; `None` ⇒ the route reports profiling unavailable. May block on
/// file I/O, so the server runs it on a blocking thread.
pub type HeapDumpFn = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// The things that vary between the worker's and the proxy's probe server.
/// Everything else is shared transport in [`ProbeServer`].
#[derive(Clone)]
pub struct ProbeRoutes {
    /// `GET /metrics` body, regenerated per scrape.
    pub metrics: MetricsFn,
    /// `GET /readyz` / `GET /ready` state.
    pub ready: ReadyFn,
    /// `GET /debug/heap` jemalloc heap dump; `None` ⇒ route 503s "unavailable".
    pub heap: Option<HeapDumpFn>,
    /// What `GET /vrrp` asks on top of liveness; `None` ⇒ the route is `404`.
    pub vrrp: Option<LiveFn>,
    /// The operator routes under `/admin/`; `None` ⇒ every one is `404`.
    pub admin: Option<AdminFn>,
}

/// A connection that sends nothing for this long is cut and its fd reclaimed.
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
        let task = tokio::spawn(accept::run(listener, routes, idle_timeout));
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
    let mut buf = [0u8; 1024];
    // Fully-idle cut: if no request line arrives within the window, drop the
    // connection (reclaim the fd). Only a connection that sent *nothing* trips
    // this — any first byte proceeds to handling.
    let n = match tokio::time::timeout(idle_timeout, stream.read(&mut buf)).await {
        Ok(read) => read?,
        Err(_elapsed) => return Ok(()), // silent client — drop
    };
    let req = String::from_utf8_lossy(buf.get(..n).unwrap_or(&buf));
    let mut request_line = req.split_whitespace();
    let method = request_line.next().unwrap_or("GET");
    let path = request_line.next().unwrap_or("/");

    if path.starts_with(admin::PREFIX) {
        let route = path.split('?').next().unwrap_or(path);
        let reply = routes.admin.as_ref().and_then(|admin| admin(method, route));
        let reply = reply.unwrap_or_else(|| AdminReply::json(404, String::new()));
        stream.write_all(reply.render().as_bytes()).await?;
        stream.flush().await?;
        return Ok(());
    }

    // On-demand CPU flamegraph (pprof SIGPROF sampling -> inferno SVG). Internal
    // debug route on the observability port — NOT the SIP datapath. Blocks for
    // the sample window on a blocking thread (the runner's async workers keep
    // serving); the profiler is built only for the request. `?seconds=N`
    // (default 20, max 120). Held past the idle window deliberately — the read
    // already completed, so the cut no longer applies.
    if path.split('?').next() == Some("/debug/flamegraph") {
        let secs = flamegraph_util::parse_seconds(path, 20, 120);
        let result = tokio::task::spawn_blocking(move || flamegraph_util::capture_svg(secs, 99))
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
        match result {
            Ok(svg) => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    svg.len()
                );
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(&svg).await?;
            }
            Err(e) => {
                let body = format!("flamegraph failed: {e}\n");
                let header = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(body.as_bytes()).await?;
            }
        }
        stream.flush().await?;
        return Ok(());
    }

    // On-demand jemalloc heap profile (prof.dump -> jeprof/pprof bytes). Internal
    // debug route, NOT the SIP datapath. Dumps the currently-LIVE sampled
    // allocations by call stack, so one capture after the leak has grown names
    // every leak source at once. Runs on a blocking thread (file I/O). `None`
    // heap provider (proxy, or a non-profiling build) -> 503.
    if path.split('?').next() == Some("/debug/heap") {
        let result = match routes.heap.clone() {
            Some(h) => tokio::task::spawn_blocking(move || h())
                .await
                .unwrap_or_else(|e| Err(format!("join error: {e}"))),
            None => {
                Err("heap profiling unavailable (non-jemalloc build or prof:false)".to_string())
            }
        };
        match result {
            Ok(bytes) => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(&bytes).await?;
            }
            Err(e) => {
                let body = format!("heap dump failed: {e}\n");
                let header = format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(body.as_bytes()).await?;
            }
        }
        stream.flush().await?;
        return Ok(());
    }

    let (status, content_type, body) = match path {
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", (routes.metrics)()),
        "/healthz" => match live.as_ref().is_none_or(|live| live()) {
            true => ("200 OK", "text/plain", "ok\n".to_string()),
            false => ("503 Service Unavailable", "text/plain", "stalled\n".to_string()),
        },
        "/vrrp" => match (&routes.vrrp, live.as_ref().is_none_or(|live| live())) {
            (None, _) => ("404 Not Found", "text/plain", "not found\n".to_string()),
            (Some(_), false) => ("503 Service Unavailable", "text/plain", "stalled\n".to_string()),
            (Some(vrrp), true) => match vrrp() {
                true => ("200 OK", "text/plain", "ok\n".to_string()),
                false => ("503 Service Unavailable", "text/plain", "not-ready\n".to_string()),
            },
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
