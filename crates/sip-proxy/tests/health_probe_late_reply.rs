//! Transparency: HealthProbe late-reply recovery + uncorrelated-reply rejection
//! — port of `tests/sip-front-proxy/transparency/health-probe-late-reply.test.ts`,
//! retargeted to the Rust correlation model.
//!
//! Regression for the k8s endurance run on 2026-05-05 (seed `1777960808113`):
//! under sustained UDP traffic the TS front proxy probe could mark every worker
//! `dead` and never recover, because its only re-marking path matched a
//! per-cycle `pendingByCallId[callId]` entry that `reapTimeouts` had *already
//! cleared* by the time the (valid) 200 OK arrived. Every late reply was
//! silently discarded; once a worker crossed `threshold` it stayed `dead`.
//!
//! The Rust port is structurally immune to that class of bug, and these tests
//! lock the two guarantees that make it so:
//!
//!   1. **Branch correlation, not an ephemeral Call-ID map.** A probe stays in
//!      `pending` keyed by its transaction Via branch until the tick-gated reap
//!      evicts it — the reap fires once per `interval_ms` and drops only probes
//!      whose deadline has already passed *at that tick*, so a probe survives in
//!      `pending` until the first tick at-or-after its deadline (this, not any
//!      `timeout_ms > interval_ms` sizing, is what gives the late reply a window
//!      to correlate). A reply that lands several ticks after the tick that sent
//!      it still correlates by branch and resets the miss counter
//!      (`handle_reply`). A reply proving liveness also retires every other
//!      in-flight probe to the same worker, so their later reaps cannot count a
//!      spurious miss against a worker that just answered. There is no per-cycle
//!      map entry to race a reap.
//!
//!   2. **Only a reply correlated to a real probe AND from the worker's current
//!      address moves health.** This is the Rust analogue of the TS "spoofed
//!      Call-ID cannot revive a dead worker; a properly-prefixed packet does",
//!      whose three cases map onto the two defensive gates in `handle_reply`:
//!        - (a) a response whose Via branch matches no `pending` probe (a
//!          forged/replayed branch) is dropped at the FIRST gate, and
//!        - (c) a branch-correlated reply from the worker's current registry
//!          address re-marks it `Alive`; both are locked by test 3.
//!        - (b) a reply that DOES correlate by branch but whose worker no longer
//!          resolves to the probed origin (the "dead-pod resurrection race") is
//!          dropped at the SECOND gate — the stale-origin check — locked by
//!          test 4.
//!
//! These run on the real clock (like the sibling `options_e2e.rs` probe tests)
//! with deliberately tiny cadences, so total wall-clock is a few seconds — well
//! under the 60 s slow-lane threshold. Determinism comes from sizing
//! `timeout_ms` strictly larger than the injected reply delay (every reply lands
//! inside its window → never a false miss) plus a `RecordingControl` that
//! asserts `Dead` was never written, even transiently.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::{
    generate_out_of_dialog_request, generate_response, ContactSpec, GenerateOutOfDialogRequestOpts,
    GenerateResponseOpts, OutOfDialogMethod, SipTransport, ViaSpec,
};
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

const WORKER_ID: &str = "w-probe";

/// Control wrapper recording every health write — lets a test assert a worker
/// was never (even transiently) marked `Dead`, the exact symptom the 2026-05-05
/// regression produced. Writes are forwarded to the real `WorkerSet` control so
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
    ep: Box<dyn UdpEndpoint>,
    reply_delay: Duration,
    gate: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let ep: Arc<dyn UdpEndpoint> = Arc::from(ep);
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if req.method != "OPTIONS" {
                continue;
            }
            if !gate.load(Ordering::SeqCst) {
                continue; // worker is "down" — swallow the probe.
            }
            // `generate_response` echoes the request's Via verbatim, so the 200
            // carries the same branch the probe is waiting on: it correlates
            // even though it is deliberately late.
            let opts = GenerateResponseOpts {
                to_tag: Some("probe-uas".into()),
                extra_headers: vec![SipHeader {
                    name: "X-Overload".into(),
                    value: "v=1; elu=0.2; gc=0.0; adm=3".into(),
                }],
                ..Default::default()
            };
            let resp = generate_response(&req, 200, "OK", &opts);
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
    let ctl = CaptureCtl { hold: hold.clone(), captured: captured.clone(), release: release.clone() };
    let task = tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut held: Vec<(Vec<u8>, std::net::SocketAddr)> = Vec::new();
        loop {
            tokio::select! {
                pkt = ep.recv() => {
                    let Some(pkt) = pkt else { return };
                    let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
                    if req.method != "OPTIONS" {
                        continue;
                    }
                    let opts = GenerateResponseOpts { to_tag: Some("probe-uas".into()), ..Default::default() };
                    let resp = generate_response(&req, 200, "OK", &opts);
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

/// Build the probe under test over a freshly-bound UAC endpoint.
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
        vec![WorkerEntry { health: WorkerHealth::Unknown, ..WorkerEntry::alive(WORKER_ID, ProxyAddr::from(worker_sock)) }],
        Clock::test_at(0),
    ))
}

// ───────────────────────────────────────────────────────────────────────────
// 1. Late replies past the reap window keep the worker alive.
//    (TS: "late replies past the reap window keep the worker alive
//          (regression: k8s endurance 2026-05-05)")
// ───────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn late_replies_past_the_reap_window_keep_the_worker_alive() {
    let h = Harness::with_transit_delay("probe-late-reply", 5);
    let (worker_ep, worker_sock) = h.bind_sut(WORKER_ID, "127.0.0.1:5071").await;

    // Reply delay (150 ms) is THREE tick intervals (50 ms) past the tick that
    // issues each probe — the reply is unambiguously "late" — yet well inside
    // the 600 ms reply window, so its branch still correlates.
    let responder = spawn_late_responder(worker_ep, Duration::from_millis(150), Arc::new(AtomicBool::new(true)));

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
        HealthProbeConfig { interval_ms: 50, timeout_ms: 600, threshold: 3 },
    )
    .await;

    // The worker answers EVERY OPTIONS, but each reply lands a handful of ticks
    // after its tick. In the TS bug the per-cycle pending entry was already
    // reaped so the reply was discarded and the miss counter walked to
    // threshold. Here the branch still correlates, so the worker reaches — and
    // stays — Alive.
    wait_for_health(&registry, WorkerHealth::Alive).await;
    assert!(observer.band_for(WORKER_ID).is_some(), "the late reply's X-Overload payload must still be applied");

    // Hold the steady state well past `threshold × interval` so a missed
    // correlation would have flipped the worker Dead.
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

// ───────────────────────────────────────────────────────────────────────────
// 2. Stable alive across sustained late replies.
//    (TS: "stable alive across 10 cycles of sustained late replies")
// ───────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn stable_alive_across_sustained_late_replies() {
    let h = Harness::with_transit_delay("probe-late-reply-sustained", 5);
    let (worker_ep, worker_sock) = h.bind_sut(WORKER_ID, "127.0.0.1:5073").await;
    let responder = spawn_late_responder(worker_ep, Duration::from_millis(150), Arc::new(AtomicBool::new(true)));

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
        HealthProbeConfig { interval_ms: 50, timeout_ms: 600, threshold: 3 },
    )
    .await;

    wait_for_health(&registry, WorkerHealth::Alive).await;

    // Sample health across 10 reply cycles (≈ one reply round-trip each). The
    // worker must read Alive at every sample, and Dead must never have been
    // written.
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

// ───────────────────────────────────────────────────────────────────────────
// 3. An uncorrelated reply cannot revive a dead worker; a correlated one does.
//    Rust analogue of the TS "spoofed Call-ID cannot revive a dead worker;
//    properly-prefixed packet does" — correlation is the transaction Via branch
//    here (not a Call-ID prefix), so the spoof is a forged branch.
// ───────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn uncorrelated_reply_cannot_revive_dead_worker_correlated_one_does() {
    let h = Harness::with_transit_delay("probe-spoof-reject", 5);
    // Step 1 injects a synthetic 200 from the worker bind that never matched an
    // inbound request on that lane → the locally-minted-tag audit fires on the
    // fixture. The probe (not a real UA) is the SUT; waive that UA-side rule.
    h.allow_violation("rfc3261.tags", "raw-injected spoof response; probe is the SUT, not a real UA");

    let (worker_ep, worker_sock) = h.bind_sut(WORKER_ID, "127.0.0.1:5075").await;
    let probe_sock: std::net::SocketAddr = "127.0.0.1:5097".parse().unwrap();

    // The responder is GATED off at first so the worker stays Dead while we
    // probe the spoof path; we flip it on for the recovery half.
    let gate = Arc::new(AtomicBool::new(false));
    // Use a separate raw handle for the synthetic spoof injection BEFORE handing
    // the endpoint to the responder task.
    let spoof_pkt = synthetic_200("z9hG4bK-totally-forged-branch", &worker_sock, &probe_sock);
    worker_ep.send_to(&spoof_pkt, probe_sock).await.unwrap();
    let responder = spawn_late_responder(worker_ep, Duration::from_millis(40), gate.clone());

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
        HealthProbeConfig { interval_ms: 20, timeout_ms: 200, threshold: 2 },
    )
    .await;

    // Force the worker Dead directly: this exercises ONLY the reply-correlation
    // path (no real OPTIONS answered while the gate is off). The probe keeps
    // fanning OPTIONS at it (it never drops a worker from the sweep), and the
    // gated-off responder swallows them, so the worker walks to Dead and the
    // forged 200 above is the only thing that could (wrongly) revive it.
    registry.set_health(WORKER_ID, WorkerHealth::Dead);
    wait_for_health(&registry, WorkerHealth::Dead).await;

    // The forged 200 correlates to no pending probe (its branch was never
    // issued), so `handle_reply` drops it: the worker stays Dead. Give the
    // packet ample time to have transited + been processed.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        registry.resolve(WORKER_ID).map(|w| w.health),
        Some(WorkerHealth::Dead),
        "an uncorrelated (forged-branch) 200 must NOT revive a dead worker"
    );
    assert!(
        !writes.lock().unwrap().iter().any(|(_, hlth)| *hlth == WorkerHealth::Alive),
        "no Alive write may result from a forged-branch reply"
    );

    // Now let the worker answer the probe's REAL OPTIONS (its branch is in
    // `pending`): a correctly-correlated 200 revives it to Alive.
    gate.store(true, Ordering::SeqCst);
    wait_for_health(&registry, WorkerHealth::Alive).await;

    probe_task.abort();
    responder.abort();
    let _ = h.finish().await;
}

// ───────────────────────────────────────────────────────────────────────────
// 4. A branch-correlated reply from a worker whose address moved cannot revive
//    it — the stale-origin guard (probe.rs `handle_reply`, the "dead-pod
//    resurrection race"). This is the Rust analogue of the TS spoof test's
//    case (b): a packet that DOES correlate (here: by Via branch; in TS: by a
//    correctly-prefixed Call-ID) but whose worker no longer resolves to the
//    probed origin must NOT move health. Case (a) [uncorrelated → drop] and
//    case (c) [correlated + current → revive] are test 3 above; this closes (b).
// ───────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn branch_correlated_reply_from_moved_worker_cannot_revive() {
    let h = Harness::with_transit_delay("probe-stale-origin", 5);
    let (worker_ep, worker_sock) = h.bind_sut(WORKER_ID, "127.0.0.1:5077").await;

    // Phase 1: immediate replies → establish a real Alive at the ORIGINAL
    // address. Phase 2: capture a branch-correct 200 in flight, which we release
    // only after moving the worker's registry address out from under it.
    let (responder, ctl) = spawn_capture_responder(worker_ep);

    let registry = unknown_registry(worker_sock);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let control = Arc::new(RecordingControl { inner: registry.control(), writes: writes.clone() });
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));

    // Long reply window (600 ms) so the captured probe stays in `pending` well
    // past the address move; short interval so it correlates quickly first.
    let probe_task = spawn_probe(
        &h,
        "127.0.0.1:5096",
        registry.clone(),
        control,
        observer,
        4,
        HealthProbeConfig { interval_ms: 30, timeout_ms: 600, threshold: 2 },
    )
    .await;

    // The worker answers at its original address → Alive (TS case (c), the
    // baseline this test then subverts).
    wait_for_health(&registry, WorkerHealth::Alive).await;

    // Capture the NEXT probe's branch-correct 200 (still addressed to the
    // original 127.0.0.1 origin) instead of answering it.
    ctl.hold.store(true, Ordering::SeqCst);
    ctl.captured.notified().await;

    // The pod is recreated at a new host: the worker's registry address moves.
    // The captured probe is still in `pending` with the OLD address recorded.
    registry.set_address(WORKER_ID, ProxyAddr::new("127.0.0.2", worker_sock.port()));

    // Drive the worker off Alive so "must not re-mark Alive" is observable. This
    // write goes straight through the registry (not the recorded probe control),
    // so `writes` stays probe-only.
    registry.set_health(WORKER_ID, WorkerHealth::Dead);
    let mark = writes.lock().unwrap().len();

    // Release the captured 200: it still correlates by transaction Via branch,
    // but its origin (127.0.0.1) no longer matches the worker's current address
    // (127.0.0.2), so the stale-origin guard drops it before any health write.
    ctl.release.notify_one();

    // Give the released reply ample time to transit + be processed.
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

/// Build a synthetic 200 OK carrying the given (forged) Via branch — the Rust
/// analogue of the TS `synthetic200(callId)`. Mirrors the probe's own OPTIONS
/// shape so the response is well-formed, but the branch is one the probe never
/// issued, so it correlates to nothing.
fn synthetic_200(branch: &str, worker_sock: &std::net::SocketAddr, probe_sock: &std::net::SocketAddr) -> Vec<u8> {
    let probe_host = probe_sock.ip().to_string();
    let probe_port = probe_sock.port();
    let worker_host = worker_sock.ip().to_string();
    let worker_port = worker_sock.port();
    let fake_options = generate_out_of_dialog_request(
        OutOfDialogMethod::Options,
        &GenerateOutOfDialogRequestOpts {
            request_uri: format!("sip:{worker_host}:{worker_port}"),
            call_id: format!("evil-not-a-probe@{worker_host}"),
            from_uri: format!("sip:probe@{probe_host}"),
            from_tag: "spoof-from".into(),
            to_uri: format!("sip:probe@{worker_host}:{worker_port}"),
            to_tag: None,
            cseq: 1,
            via: Some(ViaSpec {
                local_ip: probe_host.clone(),
                local_port: probe_port,
                transport: SipTransport::Udp,
                branch: branch.to_string(),
                custom_params: vec![],
            }),
            contact: Some(ContactSpec { user: "probe".into(), host: probe_host, port: probe_port, uri_params: vec![] }),
            max_forwards: Some(70),
            ..Default::default()
        },
    );
    let resp = generate_response(&fake_options, 200, "OK", &GenerateResponseOpts { to_tag: Some("spoof-uas".into()), ..Default::default() });
    serialize(&SipMessage::Response(resp))
}
