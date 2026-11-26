//! The health probe's reply correlation: a late reply counts, and a reply that
//! is uncorrelated or from a moved worker's old address never moves health.
//!
//! 1. A probe stays pending, keyed by its transaction Via branch, until its own
//!    reply window (`timeout_ms`) ends, however many ticks the window spans. A
//!    reply that lands ticks after the tick that sent it, inside its window,
//!    correlates and resets the miss count; it also retires the worker's other
//!    in-flight probes, so their reaps count no spurious miss.
//! 2. Only a reply that correlates to a pending probe, while the worker is still
//!    at the probed address, moves health: a reply on a forged branch is
//!    dropped, and so is a correlated reply once the worker's address moved.
//!
//! The probe, the worker and the fabric are tasks of the test's runtime on the
//! paused clock. Each `timeout_ms` exceeds the injected reply delay, so every
//! reply lands inside its window, and a `RecordingControl` asserts `Dead` was
//! never written, even transiently.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, generate_response, GenerateOutOfDialogRequestOpts,
    GenerateResponseOpts, OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::received::observed_source;
use sip_message::types::SipHeader;
use sip_message::{serialize, SipMessage, SipParser, SipStr};
use sip_net::UdpEndpoint;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

const WORKER_ID: &str = "w-probe";

/// Control wrapper recording every health write — lets a test assert a worker
/// was never (even transiently) marked `Dead`, the symptom of a discarded late
/// reply. Writes are forwarded to the real `WorkerSet` control so
/// the registry health the probe reads back stays authoritative.
struct RecordingControl {
    inner: Arc<dyn sip_proxy::registry::control::WorkerRegistryControl>,
    writes: Arc<Mutex<Vec<(String, WorkerHealth)>>>,
}

impl sip_proxy::registry::control::WorkerRegistryControl for RecordingControl {
    fn set_health(&self, worker_id: &str, health: WorkerHealth) {
        self.writes.lock().unwrap().push((worker_id.to_string(), health));
        self.inner.set_health(worker_id, health);
    }
    fn set_view(
        &self,
        worker_id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<sip_proxy::registry::Acceptance>,
    ) {
        if let Some(health) = health {
            self.writes.lock().unwrap().push((worker_id.to_string(), health));
        }
        self.inner.set_view(worker_id, health, acceptance);
    }
    fn note_late_answer(&self, worker_id: &str, fresh_for: std::time::Duration) {
        self.inner.note_late_answer(worker_id, fresh_for);
    }
}

/// Spawn a responder that answers every OPTIONS with 200 OK, but only AFTER
/// `reply_delay` — so the reply lands several ticks past the tick that issued
/// the probe ("late"), while still inside the probe's `timeout_ms` reply window
/// so its transaction Via branch still correlates. `gate`, when false, makes the
/// responder swallow OPTIONS silently (used to hold a worker `Dead`).
///
/// Each reply is delayed on its OWN spawned task, so the per-reply lateness is a
/// pipeline delay (EVERY probe is answered, just late) and does NOT throttle the
/// worker's drain rate below the probe rate — which would manufacture genuine
/// reaps (a load artifact, not the late-reply guarantee under test).
fn spawn_late_responder(
    ep: impl Into<Arc<dyn UdpEndpoint>>,
    reply_delay: Duration,
    gate: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let ep: Arc<dyn UdpEndpoint> = ep.into();
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if req.method() != "OPTIONS" {
                continue;
            }
            if !gate.load(Ordering::SeqCst) {
                continue;
            }
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probe-uas".into()),
                extra_headers: crate::common::options_capabilities()
                    .into_iter()
                    .chain([SipHeader {
                        name: "X-Overload".into(),
                        value: "v=1; elu=0.2; gc=0.0; adm=3".into(),
                    }])
                    .collect(),
                ..Default::default()
            };
            let resp = generate_response(&req, 200, "OK", &opts).unwrap();
            let raw = serialize(&SipMessage::Response(resp));
            let ep = ep.clone();
            let src = pkt.src;
            tokio::spawn(async move {
                tokio::time::sleep(reply_delay).await;
                let _ = ep.send_to(&raw, src).await;
            });
        }
    })
}

/// Handles to drive a [`spawn_capture_responder`] from the test body.
struct CaptureCtl {
    /// While false the responder answers OPTIONS immediately (establish `Alive`);
    /// once set it captures the branch-correct 200 instead of sending it.
    hold: Arc<AtomicBool>,
    /// Notified each time a reply is captured (so the test can await one).
    captured: Arc<tokio::sync::Notify>,
    /// Notify the responder to flush every captured reply out of its endpoint.
    release: Arc<tokio::sync::Notify>,
}

/// A responder with two phases, used to exercise the stale-origin guard
/// deterministically (no reliance on how many probes happen to be in flight):
///
///   * while `hold` is false it answers every OPTIONS immediately (used to
///     establish `Alive` at the worker's original address), and
///   * once `hold` is set it CAPTURES the next OPTIONS' branch-correct 200
///     instead of sending it, signalling `captured`. The test then moves the
///     worker's registry address and signals `release`, at which point the
///     responder flushes the captured 200 out of THIS endpoint — i.e. from the
///     worker's ORIGINAL address. The reply still correlates by transaction Via
///     branch, but its origin no longer matches the worker's current address.
fn spawn_capture_responder(ep: Box<dyn UdpEndpoint>) -> (tokio::task::JoinHandle<()>, CaptureCtl) {
    let ep: Arc<dyn UdpEndpoint> = Arc::from(ep);
    let hold = Arc::new(AtomicBool::new(false));
    let captured = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let ctl =
        CaptureCtl { hold: hold.clone(), captured: captured.clone(), release: release.clone() };
    let task = tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut held: Vec<(Vec<u8>, std::net::SocketAddr)> = Vec::new();
        loop {
            tokio::select! {
                pkt = ep.recv() => {
                    let Some(pkt) = pkt else { return };
                    let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
                    if req.method() != "OPTIONS" {
                        continue;
                    }
                    let opts = GenerateResponseOpts {
                        incoming_source: Some(observed_source(pkt.src)),
                        to_tag: Some("probe-uas".into()),
                        extra_headers: crate::common::options_capabilities(),
                        ..Default::default()
                    };
                    let resp = generate_response(&req, 200, "OK", &opts).unwrap();
                    let raw = serialize(&SipMessage::Response(resp));
                    if hold.load(Ordering::SeqCst) {
                        held.push((raw, pkt.src));
                        captured.notify_one();
                    } else {
                        let _ = ep.send_to(&raw, pkt.src).await;
                    }
                }
                _ = release.notified() => {
                    for (raw, src) in held.drain(..) {
                        let _ = ep.send_to(&raw, src).await;
                    }
                }
            }
        }
    });
    (task, ctl)
}

async fn spawn_probe(
    h: &Harness,
    probe_addr: &str,
    registry: Arc<SimulatedWorkerRegistry>,
    control: Arc<dyn sip_proxy::registry::control::WorkerRegistryControl>,
    observer: Arc<WorkerLoadObserver>,
    seed: u64,
    config: HealthProbeConfig,
) -> tokio::task::JoinHandle<()> {
    let (probe_ep, _probe_sock) = h.bind_sut("probe", probe_addr).await;
    let probe = HealthProbe::new(
        probe_ep,
        sip_message::CustomParser::new(),
        registry,
        control,
        observer,
        Clock::test_at(0),
        Arc::new(IdGen::seeded(seed)),
        config,
    );
    tokio::spawn(probe.run())
}

/// Poll the registry until the worker reaches `want` (or fail after ~5 s).
async fn wait_for_health(registry: &SimulatedWorkerRegistry, want: WorkerHealth) {
    for _ in 0..500 {
        if registry.resolve(WORKER_ID).map(|w| w.health) == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "worker {WORKER_ID} never reached {want:?} (now {:?})",
        registry.resolve(WORKER_ID).map(|w| w.health)
    );
}

/// A registry holding the one worker as `Unknown` — so a `wait_for_health(Alive)`
/// genuinely waits for the FIRST correlated probe reply (a pre-seeded `Alive`
/// would return instantly, before any reply, defeating the test).
fn unknown_registry(worker_sock: std::net::SocketAddr) -> Arc<SimulatedWorkerRegistry> {
    Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive(WORKER_ID, ProxyAddr::from(worker_sock))
        }],
        Clock::test_at(0),
    ))
}

/// The worker answers every OPTIONS three ticks after its tick, inside the
/// reply window: each reply correlates by branch and its `X-Overload` payload
/// applies, so the worker reaches `Alive`, stays there, and is never written
/// `Dead`.
#[tokio::test(start_paused = true)]
async fn late_replies_past_the_reap_window_keep_the_worker_alive() {
    let h = Harness::with_transit_delay("probe-late-reply", 5);
    let (worker_ep, worker_sock) = h.bind_peer(WORKER_ID, "127.0.0.1:5071").await;

    let responder = spawn_late_responder(
        worker_ep,
        Duration::from_millis(150),
        Arc::new(AtomicBool::new(true)),
    );

    let registry = unknown_registry(worker_sock);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let probe_task = spawn_probe(
        &h,
        "127.0.0.1:5099",
        registry.clone(),
        control,
        observer.clone(),
        1,
        HealthProbeConfig {
            interval_ms: 50,
            timeout_ms: 600,
            threshold: 3,
            ..HealthProbeConfig::default()
        },
    )
    .await;

    wait_for_health(&registry, WorkerHealth::Alive).await;
    assert!(
        observer.band_for(WORKER_ID).is_some(),
        "the late reply's X-Overload payload must still be applied"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(registry.resolve(WORKER_ID).map(|w| w.health), Some(WorkerHealth::Alive));
    assert!(
        !writes.lock().unwrap().iter().any(|(_, hlth)| *hlth == WorkerHealth::Dead),
        "sustained late-but-correlated replies must never mark the worker Dead"
    );

    probe_task.abort();
    responder.abort();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stable_alive_across_sustained_late_replies() {
    let h = Harness::with_transit_delay("probe-late-reply-sustained", 5);
    let (worker_ep, worker_sock) = h.bind_peer(WORKER_ID, "127.0.0.1:5073").await;
    let responder = spawn_late_responder(
        worker_ep,
        Duration::from_millis(150),
        Arc::new(AtomicBool::new(true)),
    );

    let registry = unknown_registry(worker_sock);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let probe_task = spawn_probe(
        &h,
        "127.0.0.1:5098",
        registry.clone(),
        control,
        observer,
        2,
        HealthProbeConfig {
            interval_ms: 50,
            timeout_ms: 600,
            threshold: 3,
            ..HealthProbeConfig::default()
        },
    )
    .await;

    wait_for_health(&registry, WorkerHealth::Alive).await;

    for cycle in 0..10 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            registry.resolve(WORKER_ID).map(|w| w.health),
            Some(WorkerHealth::Alive),
            "worker must stay Alive at cycle {cycle}"
        );
    }
    assert!(
        !writes.lock().unwrap().iter().any(|(_, hlth)| *hlth == WorkerHealth::Dead),
        "no Dead write across 10 cycles of sustained late replies"
    );

    probe_task.abort();
    responder.abort();
    let _ = h.finish().await;
}

/// The worker is a simulated peer whose forged 200 is the test's subject: it
/// reaches the probe while the worker is `Dead` and revives nothing; the
/// worker's first correlated reply then does.
#[tokio::test(start_paused = true)]
async fn uncorrelated_reply_cannot_revive_dead_worker_correlated_one_does() {
    let h = Harness::with_transit_delay("probe-spoof-reject", 5);

    let (worker_ep, worker_sock) = h.bind_peer(WORKER_ID, "127.0.0.1:5075").await;
    let worker_ep: Arc<dyn UdpEndpoint> = Arc::from(worker_ep);
    let probe_sock: std::net::SocketAddr = "127.0.0.1:5097".parse().unwrap();

    let gate = Arc::new(AtomicBool::new(false));
    let responder =
        spawn_late_responder(worker_ep.clone(), Duration::from_millis(40), gate.clone());

    let registry = unknown_registry(worker_sock);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let probe_task = spawn_probe(
        &h,
        "127.0.0.1:5097",
        registry.clone(),
        control,
        observer,
        3,
        HealthProbeConfig {
            interval_ms: 20,
            timeout_ms: 200,
            threshold: 2,
            ..HealthProbeConfig::default()
        },
    )
    .await;

    registry.set_health(WORKER_ID, WorkerHealth::Dead);
    wait_for_health(&registry, WorkerHealth::Dead).await;

    const FORGED: &str = "z9hG4bK-totally-forged-branch";
    worker_ep.send_to(&synthetic_200(FORGED, &worker_sock, &probe_sock), probe_sock).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let spoof = h
        .snapshot_report()
        .entries()
        .into_iter()
        .find(|e| e.to == probe_sock && String::from_utf8_lossy(&e.raw).contains(FORGED))
        .expect("the forged 200 is on the wire");
    assert!(spoof.delivered && spoof.recv_note.is_none(), "the probe read the forged 200");
    assert_eq!(
        registry.resolve(WORKER_ID).map(|w| w.health),
        Some(WorkerHealth::Dead),
        "an uncorrelated (forged-branch) 200 must NOT revive a dead worker"
    );
    assert!(
        !writes.lock().unwrap().iter().any(|(_, hlth)| *hlth == WorkerHealth::Alive),
        "no Alive write may result from a forged-branch reply"
    );

    gate.store(true, Ordering::SeqCst);
    wait_for_health(&registry, WorkerHealth::Alive).await;

    probe_task.abort();
    responder.abort();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn branch_correlated_reply_from_moved_worker_cannot_revive() {
    let h = Harness::with_transit_delay("probe-stale-origin", 5);
    let (worker_ep, worker_sock) = h.bind_peer(WORKER_ID, "127.0.0.1:5077").await;

    let (responder, ctl) = spawn_capture_responder(worker_ep);

    let registry = unknown_registry(worker_sock);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    let probe_task = spawn_probe(
        &h,
        "127.0.0.1:5096",
        registry.clone(),
        control,
        observer,
        4,
        HealthProbeConfig {
            interval_ms: 30,
            timeout_ms: 600,
            threshold: 2,
            ..HealthProbeConfig::default()
        },
    )
    .await;

    wait_for_health(&registry, WorkerHealth::Alive).await;

    ctl.hold.store(true, Ordering::SeqCst);
    ctl.captured.notified().await;

    registry.set_address(WORKER_ID, ProxyAddr::new("127.0.0.2", worker_sock.port()));

    registry.set_health(WORKER_ID, WorkerHealth::Dead);
    let mark = writes.lock().unwrap().len();

    ctl.release.notify_one();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        registry.resolve(WORKER_ID).map(|w| w.health),
        Some(WorkerHealth::Dead),
        "a branch-correlated 200 from the worker's OLD address must NOT revive the moved (recreated) pod"
    );
    assert!(
        !writes.lock().unwrap()[mark..].iter().any(|(_, hlth)| *hlth == WorkerHealth::Alive),
        "the stale-origin reply must produce no Alive write after the address moved"
    );

    probe_task.abort();
    responder.abort();
    let _ = h.finish().await;
}

/// Build a synthetic 200 OK carrying the given (forged) Via branch. Mirrors the probe's own OPTIONS
/// shape so the response is well-formed, but the branch is one the probe never
/// issued, so it correlates to nothing.
fn synthetic_200(
    branch: &str,
    worker_sock: &std::net::SocketAddr,
    probe_sock: &std::net::SocketAddr,
) -> Vec<u8> {
    let probe_host = probe_sock.ip().to_string();
    let probe_port = probe_sock.port();
    let worker_host = worker_sock.ip().to_string();
    let worker_port = worker_sock.port();
    let fake_options = generate_out_of_dialog_request(
        OutOfDialogMethod::Options,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(Uri::sip(SipStr::owned(&worker_host)).with_port(worker_port)),
            call_id: format!("evil-not-a-probe@{worker_host}"),
            from: Some(
                header::From::from_uri(Uri::sip_user(
                    SipStr::from_static("probe"),
                    SipStr::owned(&probe_host),
                ))
                .with_tag(SipStr::from_static("spoof-from")),
            ),
            to: Some(header::To::from_uri(
                Uri::sip_user(SipStr::from_static("probe"), SipStr::owned(&worker_host))
                    .with_port(worker_port),
            )),
            cseq: 1,
            via: Some(
                Via::udp(SipStr::owned(&probe_host), probe_port).with_branch(SipStr::owned(branch)),
            ),
            contact: Some(header::Contact::from_uri(
                Uri::sip_user(SipStr::from_static("probe"), SipStr::owned(&probe_host))
                    .with_port(probe_port),
            )),
            max_forwards: Some(70),
            ..Default::default()
        },
    )
    .unwrap();
    let resp = generate_response(
        &fake_options,
        200,
        "OK",
        &GenerateResponseOpts { to_tag: Some("spoof-uas".into()), ..Default::default() },
    )
    .unwrap();
    serialize(&SipMessage::Response(resp))
}
