//! [`ProbeServer`] — the one hand-rolled HTTP probe/metrics server both runner
//! binaries (the b2bua worker and the front proxy) expose on their
//! observability port. A tiny tokio TCP handler (no HTTP framework dep) serving
//! a fixed route set:
//!
//! - `GET /metrics` — Prometheus exposition, body supplied per-scrape by the
//!   injected [`ProbeRoutes::metrics`] closure (each runner concatenates its own
//!   metric sources: proxy text + txn/jemalloc for the worker, proxy text +
//!   jemalloc for the proxy).
//! - `GET /healthz` — `ok` (liveness: the process is up). Always `200`.
//! - `GET /readyz` and its `GET /ready` alias — `200`/`503` driven by the
//!   injected [`ProbeRoutes::ready`] closure's 3-valued [`ProbeState`], so the
//!   k8s readinessProbe withholds traffic from a not-ready / draining pod.
//! - `GET /debug/flamegraph[?seconds=N]` — on-demand pprof CPU flamegraph SVG
//!   (internal debug route, NOT the SIP datapath; blocks for the sample window
//!   on a blocking thread).
//! - everything else — `404`.
//!
//! ## Why one module
//! This was two near-identical hand-rolled copies (one inline in `b2bua-runner`,
//! one as `sip-proxy`'s `MetricsServer`). Only two things ever differed across
//! them — the `/metrics` body and the readiness predicate — and they are exactly
//! the two closures of [`ProbeRoutes`]. Everything else (the accept loop, the
//! exact-path match, the response framing, the **fully-idle connection cut**,
//! the flamegraph route) is shared transport that previously lived hardened in
//! one copy and un-hardened in the other. Folding it here makes the hardening
//! apply to both by construction.
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

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

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
}

/// A connection that sends nothing for this long is cut and its fd reclaimed.
/// See the module docs: this is bug-stranded-fd reclamation, not DDoS defence,
/// so it is long enough that a healthy probe/scrape never nears it.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Light backstop on concurrent connections. The cluster is internal, so this
/// is not a throttle — just a ceiling so a runaway client can't spawn unbounded
/// handler tasks. A handful of probes + one scraper + a flamegraph fit easily.
const MAX_CONNECTIONS: usize = 64;

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

    /// Like [`start`](Self::start) but with an explicit fully-idle cut window —
    /// tests pass a short one to exercise the cut without waiting [`IDLE_TIMEOUT`].
    pub async fn start_with(
        addr: std::net::SocketAddr,
        routes: ProbeRoutes,
        idle_timeout: Duration,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        let task = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue; // at the backstop cap — drop the connection
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = handle_conn(stream, routes, idle_timeout).await;
                });
            }
        });
        Ok(Self { addr: local, task })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    routes: ProbeRoutes,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    // Fully-idle cut: if no request line arrives within the window, drop the
    // connection (reclaim the fd). Only a connection that sent *nothing* trips
    // this — any first byte proceeds to handling.
    let n = match tokio::time::timeout(idle_timeout, stream.read(&mut buf)).await {
        Ok(read) => read?,
        Err(_elapsed) => return Ok(()), // silent client — drop
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");

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
            None => Err("heap profiling unavailable (non-jemalloc build or prof:false)".to_string()),
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
        "/healthz" => ("200 OK", "text/plain", "ok\n".to_string()),
        // `/ready` (worker) and `/readyz` (proxy) are the same probe.
        "/readyz" | "/ready" => match (routes.ready)() {
            ProbeState::Ready => ("200 OK", "text/plain", "ready\n".to_string()),
            ProbeState::NotReady => ("503 Service Unavailable", "text/plain", "not-ready\n".to_string()),
            ProbeState::Draining => ("503 Service Unavailable", "text/plain", "draining\n".to_string()),
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn routes(body: &'static str, ready: ReadyFn) -> ProbeRoutes {
        ProbeRoutes {
            metrics: Arc::new(move || body.to_string()),
            ready,
            heap: None,
        }
    }
    fn always(state: ProbeState) -> ReadyFn {
        Arc::new(move || state)
    }

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    #[tokio::test]
    async fn serves_metrics_body_from_the_closure() {
        let server = ProbeServer::start(
            "127.0.0.1:0".parse().unwrap(),
            routes("# TYPE demo counter\ndemo 1\n", always(ProbeState::Ready)),
        )
        .await
        .unwrap();
        let resp = get(server.addr(), "/metrics").await;
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("# TYPE demo counter"));
    }

    #[tokio::test]
    async fn healthz_is_always_ok() {
        let server = ProbeServer::start(
            "127.0.0.1:0".parse().unwrap(),
            routes("", always(ProbeState::Draining)),
        )
        .await
        .unwrap();
        // Liveness is process-up, independent of readiness — ok even while draining.
        assert!(get(server.addr(), "/healthz").await.contains("200 OK"));
    }

    #[tokio::test]
    async fn readyz_reflects_the_three_states_on_both_path_names() {
        // A flip-able source: 0=NotReady, 1=Ready, 2=Draining.
        let st = Arc::new(AtomicU8::new(1));
        let st2 = st.clone();
        let ready: ReadyFn = Arc::new(move || match st2.load(Ordering::SeqCst) {
            1 => ProbeState::Ready,
            2 => ProbeState::Draining,
            _ => ProbeState::NotReady,
        });
        let server = ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes("", ready))
            .await
            .unwrap();

        for path in ["/readyz", "/ready"] {
            st.store(1, Ordering::SeqCst);
            let r = get(server.addr(), path).await;
            assert!(r.contains("200 OK") && r.contains("ready"), "{path} ready: {r}");

            st.store(0, Ordering::SeqCst);
            let r = get(server.addr(), path).await;
            assert!(r.contains("503") && r.contains("not-ready"), "{path} not-ready: {r}");

            st.store(2, Ordering::SeqCst);
            let r = get(server.addr(), path).await;
            assert!(r.contains("503") && r.contains("draining"), "{path} draining: {r}");
        }
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let server = ProbeServer::start(
            "127.0.0.1:0".parse().unwrap(),
            routes("", always(ProbeState::Ready)),
        )
        .await
        .unwrap();
        assert!(get(server.addr(), "/nope").await.contains("404 Not Found"));
    }

    #[tokio::test]
    async fn fully_idle_connection_is_cut() {
        // A connection that sends nothing is dropped after the idle window; the
        // client observes EOF (read returns 0). Short window so the test is fast.
        let server = ProbeServer::start_with(
            "127.0.0.1:0".parse().unwrap(),
            routes("", always(ProbeState::Ready)),
            Duration::from_millis(150),
        )
        .await
        .unwrap();
        let mut s = TcpStream::connect(server.addr()).await.unwrap();
        // Send nothing. Within ~the idle window the server closes the connection.
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
            .await
            .expect("server should have cut the idle connection, not hung")
            .unwrap();
        assert_eq!(n, 0, "fully-idle connection should be closed (EOF)");
    }

    #[tokio::test]
    async fn a_connection_that_sends_a_request_is_not_cut() {
        // The cut is "fully idle only": a real request completes even with a
        // tiny idle window, because the window only guards the wait for bytes.
        let server = ProbeServer::start_with(
            "127.0.0.1:0".parse().unwrap(),
            routes("ok-body\n", always(ProbeState::Ready)),
            Duration::from_millis(150),
        )
        .await
        .unwrap();
        assert!(get(server.addr(), "/metrics").await.contains("ok-body"));
    }
}
