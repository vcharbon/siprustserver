//! The loadgen driver on the FAKE network under a PAUSED clock — the
//! callshapes-program phase-A substrate (docs/todos/callshapes-program.md §6).
//!
//! The REAL load stack — driver, mux, demux/leg-picker, correlation,
//! `DropModel` loss + `--auto-retransmit` engine — runs over the
//! `SimulatedSignalingNetwork` shared with an in-process `B2buaCore` SUT, via
//! the `MuxCore::bind_on` transport seam. No real sockets, no wall clock:
//! `start_paused` + auto-advance drive every timer (governor slots, recv
//! timeouts, retransmit ladders, the SUT's own reap timers) on virtual time,
//! so the loss tests that need 32 s+ of SIP timer traffic run deterministically
//! in the default lane.
//!
//! These mirror representative real-UDP tests in `smoke.rs` (which stay as the
//! thin real-socket lane); the paused-clock lane is where new shape coverage
//! grows (phases B–D).

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use layer_harness::TransportKind;
use loadgen::{
    CallConfig, CallTuning, Correlation, Driver, DriverCfg, DropDir, EgressPolicy, EndpointSpec,
    MixEntry, MuxCore, MuxTransport, Reporter, ReporterCfg, ResultClass, Role, ScenarioInputs,
    ShapeRegistry, TargetedDrop,
};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

/// Per-recv timeout. Virtual time: a doomed call idles this long on the
/// paused timeline (cheap), a healthy call returns at its datagram. Wide
/// enough to clear the full retransmit ladder (0.5+1+2+4 s, capped at T2)
/// several times over.
const RECV: Duration = Duration::from_secs(20);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

fn mix(id: &str, weight: f64) -> MixEntry {
    MixEntry::by_id(&ShapeRegistry::with_defaults(), id, &ScenarioInputs::default(), weight)
        .unwrap_or_else(|| panic!("unknown load shape {id:?}"))
}

/// Stand up the paused-clock rig: one simulated fabric carrying BOTH the
/// in-process b2bua SUT (bound through the recording `Harness`, so `PanicDump`
/// shows the SUT-side wire on failure) and the mux endpoints (bound raw via
/// `bind_on`; the loadgen runs its own per-call recording/audit above the mux).
async fn setup_fake(
    base: u16,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let clock = Clock::test_at(0);
    let h = Harness::with_network_and_clock(
        "loadgen-fake-net",
        sim.clone() as Arc<dyn SignalingNetwork>,
        clock.clone(),
        TransportKind::Fake,
        RECV,
    );
    h.disarm_cseq_gate(); // infra harness; loadgen runs its own per-call audit

    let (uac, uas, refer) = (base, base + 1, base + 2);
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", uas)
        .tune(|c| c.relay_headers = vec!["X-Loadgen-Id".to_string()])
        .start(&h, "b2bua", &format!("127.0.0.1:{}", base + 3))
        .await;

    let correlation = Correlation::header("X-Loadgen-Id");
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![
            EndpointSpec { addr: addr(uac), role: Role::Caller },
            EndpointSpec { addr: addr(uas), role: Role::Callee },
            EndpointSpec { addr: addr(refer), role: Role::Callee },
        ],
        correlation.clone(),
        256,
        8,
        RECV,
        clock.clone(),
    )
    .await
    .unwrap();

    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: addr(uac),
        uas_addr: addr(uas),
        refer_addr: addr(refer),
        correlation,
        recv_timeout: RECV,
        clock,
    });
    (h, b2bua, core, transport)
}

fn cfg(via: SocketAddr, cps: f64, secs: u64, mif: usize, seed: u64) -> DriverCfg {
    DriverCfg {
        cps,
        duration: Duration::from_secs(secs),
        max_in_flight: mif,
        seed,
        call: CallConfig {
            via,
            egress: EgressPolicy::Transparent,
            options_hold: Duration::from_millis(120),
            options_cadence: Duration::from_millis(40),
            ring_delay: Duration::from_millis(0),
            talk_time: Duration::from_millis(0),
            reinvite_gap: Duration::from_millis(0),
            long_hold: Duration::from_millis(120),
            teardown_quiesce: Duration::from_millis(200),
        },
        default_tuning: CallTuning::default(),
        tuning: std::collections::HashMap::new(),
    }
}

/// (a) The seam end-to-end, no loss: concurrent basic calls through the REAL
/// driver `run_one` path over the fake net all complete OK under the strict
/// per-call RFC audit, with zero orphans, no mux registry leak, and a fully
/// reaped SUT — deterministically, on virtual time.
#[tokio::test(start_paused = true)]
async fn loadgen_driver_basic_calls_on_fake_net() {
    let (_h, b2bua, core, transport) = setup_fake(7000).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 30.0, 2, 16, 0xFA4E),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    assert!(total >= 40, "governor under-delivered on virtual time: {total}");
    // Deterministic substrate → strict all-OK (the real-UDP smoke's contention
    // headroom is unnecessary here; any NOK is a genuine defect).
    assert_eq!(ok, total, "NOK calls on the fake net:\n{}", reporter.render_prometheus());
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the fake net"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// (b) Deterministic targeted loss + recovery on the fake net — the
/// paused-clock equivalent of `loadgen_settle_gate_recovers_dropped_bye`: each
/// refer call's FIRST outbound BYE is discarded before the wire; the
/// auto-retransmit Timer-E resend heals it, the settle ledger closes, and
/// EVERY call is OK with a clean audit (all-or-nothing, no dominance ratio).
#[tokio::test(start_paused = true)]
async fn loadgen_fake_net_recovers_targeted_dropped_bye() {
    let (_h, b2bua, core, transport) = setup_fake(7010).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 2.0, 2, 4, 0x2D07);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop {
            method: "BYE",
            nth: 1,
            permanent: false,
            dir: DropDir::Outbound,
            leg: None,
        }),
    };
    let driver = Driver::new(c, vec![mix("refer", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("refer", &ResultClass::Ok);
    assert!(total >= 2, "no calls ran");
    assert!(drops >= total, "the targeted BYE drop never fired: drops={drops} calls={total}");
    assert_eq!(
        ok, total,
        "a targeted first-BYE drop was not recovered by retransmit:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("refer", &ResultClass::RfcAuditFail),
        0,
        "a recovered drop leaked into the RFC audit:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// C6 (a): ten SERIALIZED re-INVITE cycles per call (the "10 re-INVITEs" ask)
/// through the real driver over the fake net. Each cycle is gated on the prior
/// one COMPLETING (`reneg_count`), so no two re-INVITEs are ever in flight (no
/// glare); every call reaches all ten renegotiations, tears down, and settles
/// OK under the strict per-call audit — deterministically on virtual time.
#[tokio::test(start_paused = true)]
async fn loadgen_fake_net_reinvite_x10_serialized() {
    let (_h, b2bua, core, transport) = setup_fake(7030).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 10.0, 2, 8, 0x1010),
        vec![mix("reinvite10", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("reinvite10", &ResultClass::Ok);
    assert!(total >= 10, "governor under-delivered: {total}");
    assert_eq!(ok, total, "a serialized ×10 re-INVITE call was NOK:\n{}", reporter.render_prometheus());
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the fake net"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// C6 (b): the ×10 re-INVITE serialization under ~6%/datagram loss +
/// auto-retransmit. A dropped re-INVITE (or its 2xx/ACK) is healed by the
/// retransmit engine + the reactor's idempotent re-ACK; the per-cycle barrier
/// simply waits, so the chain never overlaps and never wedges. No loss-model
/// drop may surface as an RFC-audit finding, and every NOK degrades to a
/// bounded `timeout` (never a protocol-defect class). Afterwards the paused
/// clock is advanced past the SUT's 32 s dead-call detection and the strict
/// release oracle gates.
#[tokio::test(start_paused = true)]
async fn loadgen_fake_net_reinvite_x10_loss_soak() {
    let (h, b2bua, core, transport) = setup_fake(7040).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 8.0, 3, 6, 0xB10C);
    c.default_tuning = CallTuning { drop_rate: 0.06, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("reinvite10", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed) + core.stats().dropped_in.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("reinvite10", &ResultClass::Ok);
    let timeouts = reporter.count("reinvite10", &ResultClass::Timeout);
    assert!(drops > 0, "loss model dropped nothing — the soak is vacuous");
    assert!(total >= 10, "too few calls: {total}");
    // A ×10 re-INVITE call carries ~10× the datagrams of a basic call, so
    // per-call loss compounds; the deterministic invariants are that retransmit
    // recovers SOME calls (the chain heals, not wedges) and every failure is a
    // bounded timeout — not that recovery dominates.
    assert!(
        ok > 0,
        "retransmit recovered NO serialized ×10 chain under loss: ok={ok} timeouts={timeouts} drops={drops}\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("reinvite10", &ResultClass::RfcAuditFail),
        0,
        "a loss-model drop surfaced as an RFC-audit finding:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        ok + timeouts,
        total,
        "a ×10 call failed with a non-timeout class under loss:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss");
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// (c) Probabilistic loss soak on the fake net: ~8%/datagram loss +
/// auto-retransmit over a basic-call mix. Seeded RNGs on a deterministic
/// substrate → the run is reproducible; a loss-model drop must NEVER surface
/// as an RFC-audit finding, and an unrecovered residue degrades to a clean
/// `timeout`, never a protocol-defect class. Afterwards the paused clock is
/// advanced past the SUT's own 32 s dead-call detection so even a
/// fully-lost-teardown straggler is reaped — then the full-reap oracle gates
/// (the CLAUDE.md termination rule, impossible to assert on the real-UDP lane
/// without a 45 s wall-clock settle).
#[tokio::test(start_paused = true)]
async fn loadgen_fake_net_loss_soak_recovers_with_retransmit() {
    let (h, b2bua, core, transport) = setup_fake(7020).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 12.0, 3, 8, 0x5EED);
    c.default_tuning = CallTuning { drop_rate: 0.08, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed) + core.stats().dropped_in.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    let timeouts = reporter.count("basic_call", &ResultClass::Timeout);
    assert!(drops > 0, "loss model dropped nothing — the soak is vacuous");
    assert!(total >= 30, "too few calls: {total}");
    assert!(
        ok >= timeouts * 4,
        "retransmit did not dominate under loss: ok={ok} timeouts={timeouts} drops={drops}\n{}",
        reporter.render_prometheus()
    );
    // The two graceful-degradation gates: no audit false positives, and every
    // NOK is a timeout (bounded give-up), never a protocol-defect class.
    assert_eq!(
        reporter.count("basic_call", &ResultClass::RfcAuditFail),
        0,
        "a loss-model drop surfaced as an RFC-audit finding:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        ok + timeouts,
        total,
        "a call failed with a non-timeout class under loss:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss");

    // Worst case is still termination: a straggler whose teardown was fully
    // lost falls back to the SUT's 32 s terminating safety timer. Drain past it
    // (chunked advance — every intermediate timer fires in order), then the
    // strict release oracle.
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}
