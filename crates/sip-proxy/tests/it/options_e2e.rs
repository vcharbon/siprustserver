//! OPTIONS end-to-end — port of
//! `tests/sip-front-proxy/integration/options-end-to-end.test.ts`, retargeted to
//! a **simulated B2BUA responder**. A real [`HealthProbe`] fans OPTIONS at a
//! worker; the responder answers as a worker does in each of its states
//! (ADR-0050 §2) or stays silent; the probe drives the worker's health and
//! acceptance through the control seam (§3): a `200` → `Alive` with the
//! acceptance its `X-Readiness` states, a `503` → refusing, health as it was,
//! repeated silence → `Dead`, a new `X-Incarnation` → `Unknown` first.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipHeader;
use sip_message::worker_report::{incarnation_value, Readiness, X_INCARNATION, X_READINESS};
use sip_message::{serialize, SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{sweep_alive, LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Serving and accepting new calls.
    Accepting,
    /// Serving, refusing new calls (`cause=backend`).
    Refusing,
    /// Not yet bootstrapped.
    NotReady,
    /// Serving and accepting, with no load payload.
    Bare,
    Silent,
}

/// What the simulated worker answers with, and the process it names.
#[derive(Clone, Copy)]
struct Worker {
    mode: Mode,
    incarnation: u64,
}

fn hdr(name: &str, value: String) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.into() }
}

/// Spawn a simulated B2BUA that answers OPTIONS per the shared `worker`.
fn spawn_responder(
    ep: Box<dyn UdpEndpoint>,
    worker: Arc<Mutex<Worker>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if req.method() != "OPTIONS" {
                continue;
            }
            let w = *worker.lock().unwrap();
            let incarnation = hdr(X_INCARNATION, incarnation_value(w.incarnation));
            let serving = |readiness: Readiness| {
                crate::common::options_capabilities()
                    .into_iter()
                    .chain([
                        hdr("X-Overload", "v=1; elu=0.2; gc=0.0; adm=3".to_string()),
                        hdr(X_READINESS, readiness.header_value()),
                        incarnation.clone(),
                    ])
                    .collect()
            };
            let (status, reason, extra): (u16, &str, Vec<SipHeader>) = match w.mode {
                Mode::Silent => continue,
                Mode::Bare => (
                    200,
                    "OK",
                    crate::common::options_capabilities()
                        .into_iter()
                        .chain([incarnation.clone()])
                        .collect(),
                ),
                Mode::Accepting => (200, "OK", serving(Readiness::accept())),
                Mode::Refusing => (200, "OK", serving(Readiness::refuse("backend"))),
                Mode::NotReady => (503, "Service Unavailable", vec![incarnation.clone()]),
            };
            let opts = GenerateResponseOpts {
                to_tag: Some("uas".into()),
                extra_headers: extra,
                ..Default::default()
            };
            let resp = generate_response(&req, status, reason, &opts).unwrap();
            let _ = ep.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    })
}

/// A probe at a 30 ms tick over one worker the registry holds `Unknown`.
struct Probed {
    h: Harness,
    registry: Arc<SimulatedWorkerRegistry>,
    observer: Arc<WorkerLoadObserver>,
    worker: Arc<Mutex<Worker>>,
    task: tokio::task::JoinHandle<()>,
}

async fn probed(name: &str, mode: Mode) -> Probed {
    let h = Harness::with_transit_delay(name, 0);
    let (worker_ep, worker_sock) = h.bind_sut("b2b-1", "127.0.0.1:5071").await;
    let worker = Arc::new(Mutex::new(Worker { mode, incarnation: 1 }));
    let _responder = spawn_responder(worker_ep, worker.clone());
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive("b2b-1", ProxyAddr::from(worker_sock))
        }],
        Clock::test_at(0),
    ));
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let (probe_ep, _probe_sock) = h.bind_sut("probe", "127.0.0.1:5099").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        registry.control(),
        observer.clone(),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(1)),
        HealthProbeConfig { interval_ms: 30, timeout_ms: 40, threshold: 2 },
    );
    let task = tokio::spawn(probe.run());
    Probed { h, registry, observer, worker, task }
}

/// The worker as the registry holds it.
fn view(registry: &SimulatedWorkerRegistry) -> (WorkerHealth, Acceptance) {
    let w = registry.resolve("b2b-1").expect("registered");
    (w.health, w.acceptance)
}

/// Poll the registry until the worker reads `want` (or fail after ~3 s).
async fn wait_for_view(registry: &SimulatedWorkerRegistry, want: (WorkerHealth, Acceptance)) {
    for _ in 0..300 {
        if view(registry) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("worker never read {want:?} (now {:?})", view(registry));
}

#[tokio::test(start_paused = true)]
async fn probe_drives_worker_health_and_acceptance_through_options() {
    let p = probed("options-e2e", Mode::Accepting).await;
    use Acceptance::{Accept, Refuse};
    use WorkerHealth::{Alive, Dead};

    // 200 accepting → Alive, accepting; the X-Overload payload reaches the observer.
    wait_for_view(&p.registry, (Alive, Accept)).await;
    assert!(p.observer.band_for("b2b-1").is_some(), "X-Overload payload should have been applied");

    // 200 refusing → still Alive, refusing.
    p.worker.lock().unwrap().mode = Mode::Refusing;
    wait_for_view(&p.registry, (Alive, Refuse)).await;
    p.worker.lock().unwrap().mode = Mode::Accepting;
    wait_for_view(&p.registry, (Alive, Accept)).await;

    // 503 → refusing, health as it was.
    p.worker.lock().unwrap().mode = Mode::NotReady;
    wait_for_view(&p.registry, (Alive, Refuse)).await;

    // Silence past the miss threshold → Dead.
    p.worker.lock().unwrap().mode = Mode::Silent;
    wait_for_view(&p.registry, (Dead, Refuse)).await;

    p.task.abort();
    let _ = p.h.finish().await;
}

/// A 503 from a worker the proxy never read alive leaves it `Unknown`.
#[tokio::test(start_paused = true)]
async fn a_503_leaves_an_unknown_worker_unknown() {
    let p = probed("options-e2e-503-unknown", Mode::NotReady).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(view(&p.registry), (WorkerHealth::Unknown, Acceptance::Refuse));
    p.task.abort();
    let _ = p.h.finish().await;
}

/// A worker restarted at the same address names a new incarnation: the
/// proxy reads it `Unknown` again, never on the previous process's `Alive`,
/// until the new process answers `200` (ADR-0050 §8).
#[tokio::test(start_paused = true)]
async fn a_new_incarnation_at_the_same_address_resets_the_worker_to_unknown() {
    let p = probed("options-e2e-incarnation", Mode::Accepting).await;
    wait_for_view(&p.registry, (WorkerHealth::Alive, Acceptance::Accept)).await;

    *p.worker.lock().unwrap() = Worker { mode: Mode::NotReady, incarnation: 2 };
    wait_for_view(&p.registry, (WorkerHealth::Unknown, Acceptance::Refuse)).await;

    p.worker.lock().unwrap().mode = Mode::Accepting;
    wait_for_view(&p.registry, (WorkerHealth::Alive, Acceptance::Accept)).await;
    p.task.abort();
    let _ = p.h.finish().await;
}

/// A restart at the same address answered `200` at once is read `Alive` in
/// one write: no reader sees the worker `Unknown` in between, which would
/// pass its in-dialog requests to its backup for nothing.
#[tokio::test(start_paused = true)]
async fn a_restart_answered_200_is_read_alive_in_one_write() {
    let h = Harness::with_transit_delay("options-e2e-restart-200", 0);
    let (worker_ep, worker_sock) = h.bind_sut("b2b-1", "127.0.0.1:5071").await;
    let worker = Arc::new(Mutex::new(Worker { mode: Mode::Accepting, incarnation: 1 }));
    let _responder = spawn_responder(worker_ep, worker.clone());
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive("b2b-1", ProxyAddr::from(worker_sock))
        }],
        Clock::test_at(0),
    ));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let (probe_ep, _probe_sock) = h.bind_sut("probe", "127.0.0.1:5099").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        control,
        observer,
        Clock::test_at(0),
        Arc::new(IdGen::seeded(4)),
        HealthProbeConfig { interval_ms: 30, timeout_ms: 40, threshold: 2 },
    );
    let task = tokio::spawn(probe.run());
    wait_for_view(&registry, (WorkerHealth::Alive, Acceptance::Accept)).await;

    writes.lock().unwrap().clear();
    worker.lock().unwrap().incarnation = 2;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = writes.lock().unwrap().clone();
    assert!(seen.contains(&WorkerHealth::Alive), "{seen:?}");
    assert!(!seen.contains(&WorkerHealth::Unknown), "never Unknown in between: {seen:?}");
    task.abort();
    let _ = h.finish().await;
}

/// Two alive workers past the stale-payload window, swept every second by
/// the runner's sweep (ADR-0050 §6): one refuses new calls and keeps sending
/// its load payload, the other accepts but sends none. The silent one is
/// floored, its bucket down to one call; the refusing one never is, its
/// bucket admitting a burst.
#[tokio::test(start_paused = true)]
async fn the_stale_sweep_floors_a_silent_worker_and_never_a_refusing_one() {
    let h = Harness::with_transit_delay("options-e2e-refusing-sweep", 0);
    let clock = Clock::test_at(0);
    let (refusing_ep, refusing_sock) = h.bind_sut("refusing", "127.0.0.1:5071").await;
    let (silent_ep, silent_sock) = h.bind_sut("silent", "127.0.0.1:5072").await;
    let worker = |mode| Arc::new(Mutex::new(Worker { mode, incarnation: 1 }));
    let _r1 = spawn_responder(refusing_ep, worker(Mode::Refusing));
    let _r2 = spawn_responder(silent_ep, worker(Mode::Bare));
    let unknown = |id: &str, sock| WorkerEntry {
        health: WorkerHealth::Unknown,
        ..WorkerEntry::alive(id, ProxyAddr::from(sock))
    };
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![unknown("refusing", refusing_sock), unknown("silent", silent_sock)],
        clock.clone(),
    ));
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let (probe_ep, _probe_sock) = h.bind_sut("probe", "127.0.0.1:5099").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        registry.control(),
        observer.clone(),
        clock.clone(),
        Arc::new(IdGen::seeded(3)),
        HealthProbeConfig::default(),
    );
    let task = tokio::spawn(probe.run());
    wait_for_health(&registry, "refusing", WorkerHealth::Alive).await;
    wait_for_health(&registry, "silent", WorkerHealth::Alive).await;

    let stale_ms = LoadObserverConfig::default().payload_stale_ms;
    let mut floored = 0;
    for _ in 0..(3 * stale_ms / 1000) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        floored += sweep_alive(&observer, registry.as_ref(), clock.now_ms());
    }
    assert!(floored > 0, "the silent worker is floored");
    let now = clock.now_ms();
    assert!(observer.try_consume_for("silent", now).is_ok());
    assert!(observer.try_consume_for("silent", now).is_err(), "floored to one call");
    for n in 0..10 {
        assert!(observer.try_consume_for("refusing", now).is_ok(), "call {n}: never floored");
    }
    task.abort();
    let _ = h.finish().await;
}

/// Poll the registry until the worker reaches `want` (or fail after ~3 s).
async fn wait_for_health(registry: &SimulatedWorkerRegistry, id: &str, want: WorkerHealth) {
    for _ in 0..300 {
        if registry.resolve(id).map(|w| w.health) == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("worker {id} never reached {want:?} (now {:?})", registry.resolve(id).map(|w| w.health));
}

/// Control wrapper recording every health write — lets the test assert a
/// worker was never (even transiently) marked Dead.
struct RecordingControl {
    inner: Arc<dyn sip_proxy::registry::control::WorkerRegistryControl>,
    writes: Arc<Mutex<Vec<WorkerHealth>>>,
}

impl sip_proxy::registry::control::WorkerRegistryControl for RecordingControl {
    fn set_health(&self, worker_id: &str, health: WorkerHealth) {
        self.writes.lock().unwrap().push(health);
        self.inner.set_health(worker_id, health);
    }
    fn set_view(
        &self,
        worker_id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) {
        self.writes.lock().unwrap().extend(health);
        self.inner.set_view(worker_id, health, acceptance);
    }
}

/// The false-Dead flap: a probe sending ONE datagram per worker per tick turns
/// a single lost packet into a full miss — with `threshold: 1` it would flip a
/// healthy worker Dead on the spot. Riding the
/// transaction layer, Timer E retransmits at T1 (500 ms) inside the reply
/// window, so a responder that drops the FIRST datagram of every transaction
/// still yields Alive and the worker is never marked Dead.
#[tokio::test(start_paused = true)]
async fn single_packet_loss_is_absorbed_by_timer_e_retransmit() {
    let h = Harness::with_transit_delay("options-retransmit", 0);
    let worker_addr = "127.0.0.1:5072";
    let (worker_ep, worker_sock) = h.bind_sut("b2b-rtx", worker_addr).await;

    // Responder that drops the first datagram of each Call-ID and answers the
    // retransmit with 200.
    let _responder = tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(pkt) = worker_ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if req.method() != "OPTIONS" {
                continue;
            }
            if seen.insert(req.call_id().clone()) {
                continue; // first transmission lost
            }
            let opts = GenerateResponseOpts {
                to_tag: Some("uas".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            let resp = generate_response(&req, 200, "OK", &opts).unwrap();
            let _ = worker_ep.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    });

    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive("b2b-rtx", ProxyAddr::from(worker_sock))
        }],
        Clock::test_at(0),
    );
    let registry = Arc::new(registry);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let (probe_ep, _probe_sock) = h.bind_sut("probe-rtx", "127.0.0.1:5098").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        control,
        observer,
        Clock::test_at(0),
        Arc::new(IdGen::seeded(2)),
        // threshold 1: a single-shot probe would flip the worker Dead on the
        // dropped first datagram alone. The 800 ms reply
        // window comfortably covers Timer E's first retransmit (T1 = 500 ms).
        HealthProbeConfig { interval_ms: 1_000, timeout_ms: 800, threshold: 1 },
    );
    let probe_task = tokio::spawn(probe.run());

    wait_for_health(&registry, "b2b-rtx", WorkerHealth::Alive).await;
    assert!(
        !writes.lock().unwrap().contains(&WorkerHealth::Dead),
        "a single lost datagram must not mark the worker Dead (Timer E retransmit covers it)"
    );

    probe_task.abort();
    let _ = h.finish().await;
}
