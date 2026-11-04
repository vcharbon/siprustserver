//! The worker's two HTTP listeners, and what they answer from. The probe
//! server (`B2BUA_METRICS`): `/ready` from its readiness and its loops,
//! `/healthz` from its loops alone, `/metrics` from every metric source of the
//! worker. The operator server (`B2BUA_ADMIN`, loopback only): the operator
//! routes (`probe::admin`) from its readiness and its live calls, which it can
//! release.

mod admin;

use std::sync::Arc;
use std::time::Duration;

use b2bua::loops::WorkerLoops;
use b2bua::repl::ReadinessState;
use b2bua::B2buaCore;
use probe_http::{AdminFn, AdminServer, LiveFn, ProbeRoutes, ProbeServer, ProbeState, ReadyFn};

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
    /// The operator server on `B2BUA_ADMIN` serves `/admin/cordon`, the
    /// operator's cordon, `/admin/release-all`, its release of every call, and
    /// `/admin/calls`, the list of the live ones; the probe server answers
    /// none of them. Hold the returned servers for the process lifetime: each
    /// accept loop aborts on drop.
    pub async fn spawn_probe_server(
        &self,
        core: &Arc<B2buaCore>,
        extra_metrics: Option<probe_http::MetricsFn>,
        heap: Option<probe_http::HeapDumpFn>,
    ) -> ProbeServers {
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
        let routes = ProbeRoutes {
            metrics: Arc::new(move || worker.body(extra_metrics.as_ref())),
            ready: ready_fn(move || core_ready.readiness_state(), core.loops().clone(), stall),
            heap,
            fit: None,
        };
        let live = live_fn(core.loops().clone(), stall);
        let admin = admin::admin_routes(
            core.readiness(),
            core.active_calls_probe(),
            live_calls(core),
            core.release_all_handle(),
        );
        ProbeServers::start(
            &self.name,
            (self.metrics_sa, routes, live),
            (self.env.admin_addr, admin),
        )
        .await
    }
}

/// The worker's probe server and its operator server, each `None` when it
/// failed to bind: the worker serves calls without them.
pub struct ProbeServers {
    pub probe: Option<ProbeServer>,
    pub admin: Option<AdminServer>,
}

impl ProbeServers {
    /// Start the probe server on `probe`'s address with its routes and
    /// liveness, and the operator server on `admin`'s loopback address. A
    /// bind failure is logged and leaves that server out.
    pub(crate) async fn start(
        service: &str,
        probe: (std::net::SocketAddr, ProbeRoutes, LiveFn),
        admin: (std::net::SocketAddr, AdminFn),
    ) -> Self {
        let (probe_at, routes, live) = probe;
        let probe = match ProbeServer::start_live(probe_at, routes, live).await {
            Ok(server) => {
                tracing::info!(
                    service,
                    addr = %server.addr(),
                    "metrics server listening (/metrics, readiness /ready, liveness /healthz)"
                );
                Some(server)
            }
            Err(e) => {
                tracing::warn!(service, addr = %probe_at, error = %e, "metrics server failed to bind");
                None
            }
        };
        let (admin_at, routes) = admin;
        let admin = match AdminServer::start(admin_at, routes).await {
            Ok(server) => {
                tracing::info!(
                    service,
                    addr = %server.addr(),
                    "operator server listening (/admin/cordon /admin/release-all /admin/calls)"
                );
                Some(server)
            }
            Err(e) => {
                tracing::warn!(service, addr = %admin_at, error = %e, "operator server failed to bind");
                None
            }
        };
        Self { probe, admin }
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

/// `GET /admin/calls`: every call the worker serves, by ref and leg Call-IDs;
/// `None` once the worker's state is gone.
fn live_calls(core: &Arc<B2buaCore>) -> admin::LiveCallsFn {
    let probe = core.live_calls_probe();
    Arc::new(move || probe().map(|calls| calls.into_iter().map(live_call).collect()))
}

/// One call as `GET /admin/calls` lists it: its a-leg's Call-ID, then each
/// b-leg's in the order the call holds them.
fn live_call(call: call::Call) -> admin::LiveCall {
    admin::LiveCall {
        call_ref: call.call_ref,
        call_ids: std::iter::once(call.a_leg.call_id)
            .chain(call.b_legs.into_iter().map(|leg| leg.call_id))
            .collect(),
    }
}

/// `/healthz`: live while no loop is stalled past `stall`.
pub(crate) fn live_fn(loops: WorkerLoops, stall: Duration) -> LiveFn {
    Arc::new(move || loops.stalled(stall).is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

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
            fit: None,
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

    async fn request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// The operator routes answer on the loopback operator server and on no
    /// path of the probe server.
    #[tokio::test]
    async fn the_operator_routes_answer_on_their_own_listener_only() {
        let parked = || tokio::spawn(std::future::pending::<()>()).abort_handle();
        let loops = WorkerLoops::new(
            (Arc::new(LoopPulse::new()), parked()),
            (Arc::new(LoopPulse::new()), parked()),
        );
        let readiness = b2bua::repl::Readiness::always_ready();
        let routes = ProbeRoutes {
            metrics: Arc::new(String::new),
            ready: ready_fn(|| ReadinessState::Ready, loops.clone(), STALL),
            heap: None,
            fit: None,
        };
        let admin = admin::admin_routes(
            readiness.clone(),
            Arc::new(|| 0),
            Arc::new(|| Some(Vec::new())),
            Arc::new(b2bua::release_all::ReleasedAll::default),
        );
        let any = "127.0.0.1:0".parse().unwrap();
        let servers =
            ProbeServers::start("w", (any, routes, live_fn(loops, STALL)), (any, admin)).await;
        let probe = servers.probe.as_ref().unwrap().addr();
        let operator = servers.admin.as_ref().unwrap().addr();

        for (method, path) in [("POST", "/admin/cordon"), ("POST", "/admin/release-all")] {
            let r = request(probe, method, path).await;
            assert!(r.starts_with("HTTP/1.1 404"), "the probe server answered {path}: {r}");
        }
        assert!(!readiness.is_cordoned());
        let r = request(operator, "POST", "/admin/cordon").await;
        assert!(r.starts_with("HTTP/1.1 204"), "{r}");
        assert!(readiness.is_cordoned());
        assert!(request(operator, "GET", "/ready").await.starts_with("HTTP/1.1 404"));
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

    /// A call the worker built from an INVITE, with one dialled b-leg, lists
    /// the INVITE's Call-ID first and the b-leg's after it.
    #[test]
    fn a_live_call_lists_its_a_leg_call_id_then_its_b_legs() {
        use sip_message::parser::custom::CustomParser;
        use sip_message::{SipMessage, SipParser};
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-live\r\n\
            Max-Forwards: 70\r\n\
            From: <sip:alice@example.com>;tag=alicetag\r\n\
            To: <sip:bob@example.com>\r\n\
            Call-ID: a-leg@10.0.0.9\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:alice@10.0.0.9:5060>\r\n\
            Content-Length: 0\r\n\r\n";
        let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
            panic!("a request")
        };
        let cfg = b2bua::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut call = b2bua::initial_invite::build_initial_call(
            &req,
            std::net::SocketAddr::from(([10, 0, 0, 9], 5060)),
            &cfg,
            &mut sip_ids::TurnIds::fixed(1),
            1_000,
        );
        call.b_legs.push(call::testkit::dialled_leg("b-1", "btag"));
        let listed = live_call(call.clone());
        assert_eq!(listed.call_ref, call.call_ref);
        assert_eq!(listed.call_ids, ["a-leg@10.0.0.9", "b-1-call-id"]);
    }
}
