//! The proxy's probe answers over a real probe server: a stalled recv shard
//! fails liveness as well as readiness, an idle proxy fails neither; before
//! its first worker view, with no worker alive, or with its external face
//! unusable the proxy is not ready, and it is live all along. `/fitz` asks
//! the same as `/readyz` with the isolation verdict in place of the drain.

use probe_http::{ProbeRoutes, ProbeServer};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth};
use sip_proxy::ProxyAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::external_face::FaceCheck;

const STALL_MS: u64 = 2_000;
const ISOLATION: Duration = Duration::from_millis(2_000);

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

/// One worker `w0` read as `health`.
fn pool(health: WorkerHealth) -> Arc<dyn WorkerRegistry> {
    Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry {
        health,
        ..WorkerEntry::alive("w0", ProxyAddr::new("10.0.0.1", 5060))
    }]))
}

/// The probe answers' sources for `pool`, `pulse` and `view`, read at 10 s
/// on the test clock, with no external face check, a worker answer heard
/// just now and no drain.
fn sources(
    pool: Arc<dyn WorkerRegistry>,
    pulse: Arc<ShardPulse>,
    view: Arc<FirstView>,
) -> ProbeSources {
    let reach = WorkerReach::unheard();
    reach.answered();
    ProbeSources {
        draining: Arc::new(AtomicBool::new(false)),
        pulse,
        clock: Clock::test_at(10_000),
        stall_ms: STALL_MS,
        registry: pool,
        first_view: view,
        external_face: None,
        reach,
        isolation_window: ISOLATION,
    }
}

/// A probe server over [`probes`] of `sources`.
async fn serve(sources: ProbeSources) -> ProbeServer {
    let p = probes(sources);
    let routes = ProbeRoutes {
        metrics: Arc::new(String::new),
        ready: p.ready,
        heap: None,
        fit: Some(p.fit),
    };
    ProbeServer::start_live("127.0.0.1:0".parse().unwrap(), routes, p.live).await.unwrap()
}

async fn server_over(
    pool: Arc<dyn WorkerRegistry>,
    pulse: Arc<ShardPulse>,
    view: Arc<FirstView>,
) -> ProbeServer {
    serve(sources(pool, pulse, view)).await
}

async fn server_with_face(
    pool: Arc<dyn WorkerRegistry>,
    pulse: Arc<ShardPulse>,
    view: Arc<FirstView>,
    face: Option<Arc<FaceMonitor>>,
) -> ProbeServer {
    serve(ProbeSources { external_face: face, ..sources(pool, pulse, view) }).await
}

/// A probe server with one `Alive` worker and its first view in.
async fn server(pulse: Arc<ShardPulse>) -> ProbeServer {
    server_over(pool(WorkerHealth::Alive), pulse, FirstView::given()).await
}

fn is_ok(resp: &str) -> bool {
    resp.starts_with("HTTP/1.1 200")
}

/// A shard stalled past its threshold fails `/healthz`, which the
/// livenessProbe reads, `/readyz`, which the announcement follows, and
/// `/fitz`, which the route guard follows.
#[tokio::test]
async fn a_stalled_shard_fails_healthz_readyz_and_fitz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(1, 10_000 - STALL_MS - 1);
    let s = server(pulse).await;
    for path in ["/healthz", "/readyz", "/fitz"] {
        let resp = get(s.addr(), path).await;
        assert!(resp.starts_with("HTTP/1.1 503"), "{path}: {resp}");
    }
}

/// Shards waiting on their sockets, however long, or busy inside the
/// threshold: every probe passes. The busy shard took its packet half the
/// threshold before the reads, so the real time the server and the reads
/// take never carries it past the threshold.
#[tokio::test]
async fn an_idle_or_turning_proxy_passes_healthz_and_readyz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(0, 10_000 - STALL_MS / 2);
    let s = server(pulse).await;
    for path in ["/healthz", "/readyz"] {
        assert!(is_ok(&get(s.addr(), path).await), "{path}");
    }
}

/// A starting proxy, its worker already read alive: `/readyz` fails until
/// the first worker view is in; `/healthz` passes from the start, so the
/// livenessProbe never restarts it for want of a view.
#[tokio::test]
async fn before_the_first_view_readyz_fails_and_healthz_passes() {
    let view = FirstView::pending();
    let s =
        server_over(pool(WorkerHealth::Alive), Arc::new(ShardPulse::new(1)), view.clone()).await;
    assert!(is_ok(&get(s.addr(), "/healthz").await));
    let ready = get(s.addr(), "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 503") && ready.contains("not-ready"), "{ready}");

    view.latch();
    for path in ["/healthz", "/readyz"] {
        assert!(is_ok(&get(s.addr(), path).await), "{path}");
    }
}

/// The first view in and no worker alive (none answered the first round):
/// the proxy is live and not ready, so its node announces nothing
/// (ADR-0056).
#[tokio::test]
async fn with_the_view_in_and_no_worker_alive_readyz_fails() {
    let s =
        server_over(pool(WorkerHealth::Unknown), Arc::new(ShardPulse::new(1)), FirstView::given())
            .await;
    assert!(is_ok(&get(s.addr(), "/healthz").await));
    assert!(get(s.addr(), "/readyz").await.starts_with("HTTP/1.1 503"));
}

/// Two workers whose endpoints read not ready, `w0` read as `health`.
fn pool_none_ready(health: WorkerHealth) -> Arc<dyn WorkerRegistry> {
    let unprobed = |id, host| WorkerEntry {
        health: WorkerHealth::Unknown,
        ..WorkerEntry::alive(id, ProxyAddr::new(host, 5060))
    };
    let registry =
        SimulatedWorkerRegistry::new(vec![unprobed("w0", "10.0.0.1"), unprobed("w1", "10.0.0.2")]);
    registry.set_ready("w0", false);
    registry.set_ready("w1", false);
    registry.set_health("w0", health);
    Arc::new(registry)
}

/// No endpoint reads ready and one worker answers: the proxy stays ready. A
/// fresh rollout, every worker booting and none answering, stays no worker.
#[test]
fn with_no_endpoint_ready_readiness_follows_the_workers_answers() {
    let answering = pool_none_ready(WorkerHealth::Alive);
    assert_eq!(probe_state(false, false, true, true, answering.as_ref()), ProbeState::Ready);
    assert_eq!(fitness(false, true, true, false, answering.as_ref()), Ok(()));
    let booting = pool_none_ready(WorkerHealth::Unknown);
    assert_eq!(probe_state(false, false, true, true, booting.as_ref()), ProbeState::NotReady);
    assert_eq!(fitness(false, true, true, false, booting.as_ref()), Err(Unfit::NoWorkerAlive));
}

/// Every other condition met, an external face read unusable fails
/// `/readyz` and leaves `/healthz` passing: the node withdraws both faces'
/// addresses and the process is not restarted. `/readyz` reads the monitor's
/// latest reading, which is unusable before the first one completes.
#[tokio::test]
async fn an_unusable_external_face_fails_readyz_only() {
    let face =
        FaceMonitor::new(FaceCheck { interface: "readyz-test-ext0".into(), next_hops: vec![] });
    let s = server_with_face(
        pool(WorkerHealth::Alive),
        Arc::new(ShardPulse::new(1)),
        FirstView::given(),
        Some(face),
    )
    .await;
    assert!(is_ok(&get(s.addr(), "/healthz").await));
    let ready = get(s.addr(), "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 503") && ready.contains("not-ready"), "{ready}");
}

/// The readiness verdict over its inputs: the external face is one more
/// NotReady cause, below a latched drain.
#[test]
fn an_unusable_external_face_is_not_ready_and_a_drain_stays_draining() {
    let alive = pool(WorkerHealth::Alive);
    assert_eq!(probe_state(false, false, true, true, alive.as_ref()), ProbeState::Ready);
    assert_eq!(probe_state(false, false, true, false, alive.as_ref()), ProbeState::NotReady);
    assert_eq!(probe_state(true, false, true, false, alive.as_ref()), ProbeState::Draining);
}

/// Readiness counts `Alive` workers whatever their acceptance: a cluster
/// refusing new calls everywhere keeps its proxy ready (ADR-0050 §11).
#[test]
fn readiness_ignores_acceptance() {
    let entry = |health, acceptance| WorkerEntry {
        health,
        acceptance,
        ..WorkerEntry::alive("w0", ProxyAddr::new("10.0.0.1", 5060))
    };
    let state = |e: WorkerEntry| {
        probe_state(false, false, true, true, &StaticWorkerRegistry::from_entries(vec![e]))
    };
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Refuse)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Accept)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Unknown, Acceptance::Accept)), ProbeState::NotReady);
    assert_eq!(state(entry(WorkerHealth::Dead, Acceptance::Accept)), ProbeState::NotReady);
}

/// Every `/readyz` condition met, no worker answer heard for the isolation
/// window: `/fitz` names the isolation, `/readyz` still passes, so only the
/// route guard acts on it.
#[tokio::test(start_paused = true)]
async fn an_isolated_proxy_is_unfit_and_still_ready() {
    let src = sources(pool(WorkerHealth::Alive), Arc::new(ShardPulse::new(1)), FirstView::given());
    let reach = src.reach.clone();
    let p = probes(src);
    assert_eq!((p.fit)(), Fitness::Fit);
    tokio::time::sleep(ISOLATION + Duration::from_millis(1)).await;
    assert_eq!((p.fit)(), Fitness::Unfit("isolated"));
    assert_eq!((p.ready)(), ProbeState::Ready);
    reach.answered();
    assert_eq!((p.fit)(), Fitness::Fit, "one answer restores the fitness");
}

/// `/fitz` over HTTP: fit while `/readyz` reads Draining, since the drain is
/// the readiness path's; unfit naming the first `/readyz` cause otherwise.
#[tokio::test]
async fn fitz_ignores_the_drain_and_names_the_first_cause() {
    let src =
        sources(pool(WorkerHealth::Alive), Arc::new(ShardPulse::new(1)), FirstView::pending());
    let view = src.first_view.clone();
    let draining = src.draining.clone();
    let s = serve(src).await;
    let fit = get(s.addr(), "/fitz").await;
    assert!(fit.starts_with("HTTP/1.1 503") && fit.ends_with("unfit no-first-view\n"), "{fit}");
    view.latch();
    draining.store(true, Ordering::Relaxed);
    assert!(get(s.addr(), "/readyz").await.contains("draining"));
    assert!(is_ok(&get(s.addr(), "/fitz").await));
}

/// The fitness verdict over its inputs: each `/readyz` cause in its order,
/// then the isolation.
#[test]
fn fitness_names_each_cause_in_order() {
    let alive = pool(WorkerHealth::Alive);
    let dead = pool(WorkerHealth::Dead);
    assert_eq!(fitness(false, true, true, false, alive.as_ref()), Ok(()));
    assert_eq!(fitness(true, false, false, true, dead.as_ref()), Err(Unfit::ShardStalled));
    assert_eq!(fitness(false, false, false, true, dead.as_ref()), Err(Unfit::NoFirstView));
    assert_eq!(fitness(false, true, false, true, dead.as_ref()), Err(Unfit::ExternalFace));
    assert_eq!(fitness(false, true, true, true, dead.as_ref()), Err(Unfit::NoWorkerAlive));
    assert_eq!(fitness(false, true, true, true, alive.as_ref()), Err(Unfit::Isolated));
}
