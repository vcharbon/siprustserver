//! The worker's probe server, and what it answers from: `/ready` from its
//! readiness and its loops, `/healthz` from its loops alone, `/metrics` from
//! every metric source of the worker, the operator routes ([`admin`]) from its
//! readiness and its live calls.

mod admin;

use std::sync::Arc;
use std::time::Duration;

use b2bua::loops::WorkerLoops;
use b2bua::repl::ReadinessState;
use b2bua::B2buaCore;
use probe_http::{LiveFn, ProbeState, ReadyFn};

use crate::{RunnerBase, WorkerMetrics};

impl RunnerBase {
    /// Readiness/metrics probe server (shared `probe-http`). The `/ready` state
    /// is a single read of one source: `core.readiness_state()` already folds
    /// in the Draining latch, so there is no second flag to drift from it. The
    /// `/metrics` body concatenates the worker's metric sources per scrape
    /// (core registry + txn backpressure + UDP transport + overload signal +
    /// capacity gate + every new call's admission outcome, every rung
    /// composed, the loops' stall gauge, the cordon gauge), then
    /// `extra_metrics` (e.g. allocator stats). `/ready` reads NotReady and
    /// `/healthz` 503 while a worker loop is stalled past `loop_stall`.
    /// `/admin/cordon` is the operator's cordon. Hold the returned server for
    /// the process lifetime — its accept loop aborts on drop.
    pub async fn spawn_probe_server(
        &self,
        core: &Arc<B2buaCore>,
        extra_metrics: Option<probe_http::MetricsFn>,
        heap: Option<probe_http::HeapDumpFn>,
    ) -> Option<probe_http::ProbeServer> {
        let core_ready = core.clone();
        let stall = self.env.loop_stall;
        let worker = WorkerMetrics {
            core: self.metrics.clone(),
            txn: core.txn_metrics().clone(),
            udp: self.udp_metrics.clone(),
            overload: core.overload().clone(),
            capacity: core.capacity().clone(),
            loops: core.loops().clone(),
            loop_stall: stall,
            readiness: core.readiness(),
        };
        let routes = probe_http::ProbeRoutes {
            metrics: Arc::new(move || worker.body(extra_metrics.as_ref())),
            ready: ready_fn(move || core_ready.readiness_state(), core.loops().clone(), stall),
            heap,
            vrrp: None,
            admin: Some(admin::admin_routes(core.readiness(), core.active_calls_probe())),
        };
        let live = live_fn(core.loops().clone(), stall);
        match probe_http::ProbeServer::start_live(self.metrics_sa, routes, live).await {
            Ok(server) => {
                tracing::info!(
                    service = %self.name,
                    addr = %server.addr(),
                    "metrics server listening (/metrics, readiness /ready, liveness /healthz, operator /admin/cordon)"
                );
                Some(server)
            }
            Err(e) => {
                tracing::warn!(
                    service = %self.name,
                    addr = %self.metrics_sa,
                    error = %e,
                    "metrics server failed to bind"
                );
                None
            }
        }
    }
}

/// `/ready`: Draining whatever the loops do; NotReady while a loop is stalled
/// past `stall`; otherwise the readiness itself, a cordoned node reading
/// Ready (it serves its dialogs and stays published).
pub(crate) fn ready_fn(
    readiness: impl Fn() -> ReadinessState + Send + Sync + 'static,
    loops: WorkerLoops,
    stall: Duration,
) -> ReadyFn {
    Arc::new(move || match readiness() {
        ReadinessState::Draining => ProbeState::Draining,
        _ if !loops.stalled(stall).is_empty() => ProbeState::NotReady,
        ReadinessState::Ready | ReadinessState::Cordoned => ProbeState::Ready,
        ReadinessState::NotReady => ProbeState::NotReady,
    })
}

/// `/healthz`: live while no loop is stalled past `stall`.
pub(crate) fn live_fn(loops: WorkerLoops, stall: Duration) -> LiveFn {
    Arc::new(move || loops.stalled(stall).is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use probe_http::{ProbeRoutes, ProbeServer};
    use sip_txn::LoopPulse;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    const STALL: Duration = Duration::from_millis(1);

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// A stalled loop turns `/ready` NotReady and `/healthz` 503, Draining
    /// stays Draining, and both recover once the loop turns again.
    #[tokio::test]
    async fn a_stalled_loop_fails_ready_and_healthz_and_draining_stays_draining() {
        let parked = || tokio::spawn(std::future::pending::<()>()).abort_handle();
        let router = Arc::new(LoopPulse::new());
        let loops =
            WorkerLoops::new((Arc::new(LoopPulse::new()), parked()), (router.clone(), parked()));
        let state = Arc::new(Mutex::new(ReadinessState::Ready));
        let reads = state.clone();
        let ready = ready_fn(move || *reads.lock().unwrap(), loops.clone(), STALL);
        let live = live_fn(loops, STALL);
        let routes = ProbeRoutes {
            metrics: Arc::new(String::new),
            ready: ready.clone(),
            heap: None,
            vrrp: None,
            admin: None,
        };
        let server =
            ProbeServer::start_live("127.0.0.1:0".parse().unwrap(), routes, live).await.unwrap();
        assert!(get(server.addr(), "/ready").await.contains("200 OK"));
        assert!(get(server.addr(), "/healthz").await.contains("200 OK"));

        router.turning();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(ready(), ProbeState::NotReady);
        let r = get(server.addr(), "/ready").await;
        assert!(r.contains("503") && r.contains("not-ready"), "{r}");
        let r = get(server.addr(), "/healthz").await;
        assert!(r.contains("503") && r.contains("stalled"), "{r}");

        *state.lock().unwrap() = ReadinessState::Draining;
        assert_eq!(ready(), ProbeState::Draining, "Draining takes precedence");

        router.waiting();
        *state.lock().unwrap() = ReadinessState::Ready;
        assert!(get(server.addr(), "/ready").await.contains("200 OK"));
        assert!(get(server.addr(), "/healthz").await.contains("200 OK"));
    }

    /// A cordoned worker reads Ready: it serves its dialogs and stays
    /// published.
    #[tokio::test]
    async fn a_cordoned_worker_reads_ready() {
        let parked = || tokio::spawn(std::future::pending::<()>()).abort_handle();
        let loops = WorkerLoops::new(
            (Arc::new(LoopPulse::new()), parked()),
            (Arc::new(LoopPulse::new()), parked()),
        );
        let ready = ready_fn(|| ReadinessState::Cordoned, loops, STALL);
        assert_eq!(ready(), ProbeState::Ready);
    }
}
