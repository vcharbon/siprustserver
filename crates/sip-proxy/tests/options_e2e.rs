//! OPTIONS end-to-end — port of
//! `tests/sip-front-proxy/integration/options-end-to-end.test.ts`, retargeted to
//! a **simulated B2BUA responder** (the real B2BUA OPTIONS handler is unported).
//! A real [`HealthProbe`] fans OPTIONS at a worker; the responder answers 200 /
//! 503+Reason / silence; the probe drives the worker's health through the
//! control seam: 200 → Alive, 503 draining → Draining, 503 not-ready →
//! NotReady, repeated silence → Dead.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipHeader;
use sip_message::{serialize, SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok200,
    Draining503,
    NotReady503,
    Silent,
}

/// Spawn a simulated B2BUA that answers OPTIONS per the shared `mode`.
fn spawn_responder(ep: Box<dyn UdpEndpoint>, mode: Arc<Mutex<Mode>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if req.method != "OPTIONS" {
                continue;
            }
            let m = *mode.lock().unwrap();
            let (status, reason, extra): (u16, &str, Vec<SipHeader>) = match m {
                Mode::Silent => continue,
                Mode::Ok200 => (200, "OK", vec![SipHeader { name: "X-Overload".into(), value: "v=1; elu=0.2; gc=0.0; adm=3".into() }]),
                Mode::Draining503 => (503, "Service Unavailable", vec![SipHeader { name: "Reason".into(), value: "SIP;cause=503;text=\"draining\"".into() }]),
                Mode::NotReady503 => (503, "Service Unavailable", vec![SipHeader { name: "Reason".into(), value: "SIP;cause=503;text=\"not-ready (boot drain)\"".into() }]),
            };
            let opts = GenerateResponseOpts { to_tag: Some("uas".into()), extra_headers: extra, ..Default::default() };
            let resp = generate_response(&req, status, reason, &opts);
            let _ = ep.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    })
}

#[tokio::test]
async fn probe_drives_worker_health_through_options() {
    let h = Harness::with_transit_delay("options-e2e", 0);
    let worker_addr = "127.0.0.1:5071";
    let (worker_ep, worker_sock) = h.bind_sut("b2b-1", worker_addr).await;
    let mode = Arc::new(Mutex::new(Mode::Ok200));
    let _responder = spawn_responder(worker_ep, mode.clone());

    // Worker starts Unknown; the probe must observe it before it is routable.
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry { health: WorkerHealth::Unknown, ..WorkerEntry::alive("b2b-1", ProxyAddr::from(worker_sock)) }],
        Clock::test_at(0),
    );
    let registry = Arc::new(registry);
    let control = registry.control();
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let (probe_ep, _probe_sock) = h.bind_sut("probe", "127.0.0.1:5099").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        control,
        observer.clone(),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(1)),
        HealthProbeConfig { interval_ms: 30, timeout_ms: 40, threshold: 2 },
    );
    let probe_task = tokio::spawn(probe.run());

    // One tick of 200 OK → Alive, and the X-Overload payload reaches the observer.
    wait_for_health(&registry, "b2b-1", WorkerHealth::Alive).await;
    assert!(observer.band_for("b2b-1").is_some(), "X-Overload payload should have been applied");

    // 503 draining → Draining.
    *mode.lock().unwrap() = Mode::Draining503;
    wait_for_health(&registry, "b2b-1", WorkerHealth::Draining).await;

    // 503 not-ready → NotReady.
    *mode.lock().unwrap() = Mode::NotReady503;
    wait_for_health(&registry, "b2b-1", WorkerHealth::NotReady).await;

    // Silence past the miss threshold → Dead.
    *mode.lock().unwrap() = Mode::Silent;
    wait_for_health(&registry, "b2b-1", WorkerHealth::Dead).await;

    probe_task.abort();
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
}

/// Regression for the false-Dead flap: the old probe sent ONE datagram per
/// worker per tick, so a single lost packet was a full miss — with
/// `threshold: 1` it would flip a healthy worker Dead on the spot. Riding the
/// transaction layer, Timer E retransmits at T1 (500 ms) inside the reply
/// window, so a responder that drops the FIRST datagram of every transaction
/// still yields Alive and the worker is never marked Dead.
#[tokio::test]
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
            if req.method != "OPTIONS" {
                continue;
            }
            if seen.insert(req.call_id.clone()) {
                continue; // first transmission lost
            }
            let opts = GenerateResponseOpts { to_tag: Some("uas".into()), ..Default::default() };
            let resp = generate_response(&req, 200, "OK", &opts);
            let _ = worker_ep.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    });

    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry { health: WorkerHealth::Unknown, ..WorkerEntry::alive("b2b-rtx", ProxyAddr::from(worker_sock)) }],
        Clock::test_at(0),
    );
    let registry = Arc::new(registry);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl {
        inner: registry.control(),
        writes: writes.clone(),
    });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let (probe_ep, _probe_sock) = h.bind_sut("probe-rtx", "127.0.0.1:5098").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        control,
        observer,
        Clock::test_at(0),
        Arc::new(IdGen::seeded(2)),
        // threshold 1: under the old single-shot design the dropped first
        // datagram alone would have flipped the worker Dead. The 800 ms reply
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
