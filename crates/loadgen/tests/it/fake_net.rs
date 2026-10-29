//! The loadgen driver on the FAKE network under a PAUSED clock.
//!
//! The REAL load stack — driver, mux, demux/leg-picker, correlation,
//! `DropModel` loss + `--auto-retransmit` engine — runs over the
//! `SimulatedSignalingNetwork` shared with an in-process `B2buaCore` SUT, via
//! the `MuxCore::bind_on` transport seam. No real sockets, no wall clock:
//! `start_paused` + auto-advance drive every timer (governor slots, recv
//! timeouts, retransmit ladders, the SUT's own reap timers) on virtual time,
//! so the loss tests that need 32 s+ of SIP timer traffic run deterministically.
//!
//! These mirror representative real-UDP tests in `smoke.rs` (which stay as the
//! thin real-socket lane); the paused-clock lane is where new shape coverage
//! grows (phases B–D).

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::{
    CallTuning, Driver, DropDir, EgressPolicy, Reporter, ReporterCfg, ResultClass, TargetedDrop,
};

use crate::fake_rig::*;

/// (a) The seam end-to-end, no loss: concurrent basic calls through the REAL
/// driver `run_one` path over the fake net all complete OK under the strict
/// per-call RFC audit, with zero orphans, no mux registry leak, and a fully
/// reaped SUT — deterministically, on virtual time.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
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
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The sum of `metric`'s samples under stream `main` and shape `<scenario>`
/// in a Prometheus text dump.
fn scenario_sum(prom: &str, metric: &str, scenario: &str) -> u64 {
    let prefix = format!("{metric}{{stream=\"main\",shape=\"{scenario}\",");
    prom.lines()
        .filter(|l| l.starts_with(&prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

/// The cleanup-coverage mix ends each call on the ending its shape declares:
/// the callee's 486, the caller's CANCEL after ringing, the transfer target's
/// decline, and the answered call. Every one of them classes `ok`, and
/// `loadgen_branch_total{stream,shape,branch}` counts each call under its
/// scenario's accepted branch.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_abnormal_end_mix_classes_ok_by_branch() {
    let (h, b2bua, core, transport) = setup_fake(7400).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 1 }));
    let shapes = ["invite_reject", "abandon_ringing", "refer_charlie_reject", "basic_call"];
    let mut c = cfg(b2bua.addr, 20.0, 2, 16, 0xAB0E);
    // A ring long enough that the caller's CANCEL always precedes the answer.
    c.call.ring_delay = Duration::from_millis(500);
    let driver =
        Driver::new(c, shapes.iter().map(|s| mix(s, 1.0)).collect(), reporter.clone(), transport);
    driver.run().await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 20, "governor under-delivered on virtual time: {total}");
    for s in shapes {
        let ok = reporter.count(s, &ResultClass::Ok);
        assert!(ok > 0, "no {s} call classed ok:\n{prom}");
        assert_eq!(
            scenario_sum(&prom, "loadgen_branch_total", s),
            ok,
            "loadgen_branch_total counts every ok {s} call under its branch:\n{prom}"
        );
    }
    let ok: u64 = shapes.iter().map(|s| reporter.count(s, &ResultClass::Ok)).sum();
    assert_eq!(ok, total, "every declared ending classes ok:\n{prom}");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// A CPS bucket of one token that never refills: spent, it refuses every new call.
fn one_token(c: &mut b2bua_sdk::B2buaConfig) {
    c.cps_bucket_size = 1;
    c.cps_bucket_rate = 0;
}

/// A failed call is counted by the step it failed at. An SUT whose admission
/// bucket is empty refuses every non-emergency INVITE with 503, which a run
/// declaring no overload refusal counts as a failure of alice's INVITE (goal
/// #0) while the controller waits on the `established` barrier, so
/// `loadgen_failed_step_total` counts every call under
/// `alice:invite#0@established`.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_counts_failures_by_failed_step() {
    let (_h, b2bua, core, transport) = setup_fake_tuned(7410, one_token).await;
    b2bua.overload().spend_token();
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 1 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 20.0, 1, 16, 0x503),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 10, "governor under-delivered on virtual time: {total}");
    assert_eq!(
        reporter.count("basic_call", &ResultClass::WrongStatus(503)),
        total,
        "every refused call is a status_503 failure:\n{prom}"
    );
    let line = format!(
        "loadgen_failed_step_total{{stream=\"main\",shape=\"basic_call\",step=\"alice:invite#0@established\"}} {total}"
    );
    assert!(prom.lines().any(|l| l == line), "expected `{line}` in:\n{prom}");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The same refusals in a run that declares 503 as its SUT's overload refusal:
/// a 503 to a non-emergency call before it connects is that call's
/// `refused_overload` branch, so every call classes `ok` and the SUT still
/// reaps each one.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_overload_refusal_is_ok_when_expected() {
    let (_h, b2bua, core, transport) = setup_fake_tuned(7420, one_token).await;
    b2bua.overload().spend_token();
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 1 }));
    let mut c = cfg(b2bua.addr, 20.0, 1, 16, 0x503E);
    c.overload_reject = Some(503);
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 10, "governor under-delivered on virtual time: {total}");
    assert_eq!(reporter.count("basic_call", &ResultClass::Ok), total, "{prom}");
    let line = format!(
        "loadgen_branch_total{{stream=\"main\",shape=\"basic_call\",branch=\"refused_overload\"}} {total}"
    );
    assert!(prom.lines().any(|l| l == line), "expected `{line}` in:\n{prom}");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// (b) Deterministic targeted loss + recovery on the fake net — the
/// paused-clock equivalent of `loadgen_settle_gate_recovers_dropped_bye`: each
/// refer call's FIRST outbound BYE is discarded before the wire; the
/// auto-retransmit Timer-E resend heals it, the settle ledger closes, and
/// EVERY call is OK with a clean audit (all-or-nothing, no dominance ratio).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
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
        ok,
        total,
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
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C3 (a): crossing BYE through the SUT — the caller and the winning callee
/// both hang up at once (RFC 3261 §15.1.2), so the two BYEs cross in the
/// B2BUA. Every call terminates and settles OK under the strict per-call audit,
/// with no orphan and a fully-reaped SUT — deterministically on virtual time.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_crossing_bye() {
    let (_h, b2bua, core, transport) = setup_fake(7050).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 20.0, 2, 8, 0xC705),
        vec![mix("crossing_bye", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("crossing_bye", &ResultClass::Ok);
    assert!(total >= 10, "governor under-delivered: {total}");
    assert_eq!(ok, total, "a crossing-BYE call was NOK:\n{}", reporter.render_prometheus());
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the fake net"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C2/E5: CANCEL×200 crossing through the SUT. Each call ends on one of its
/// RFC-legal endings, all accepted and classed `ok`: `answered_despite_cancel`
/// (the 200 left before the CANCEL arrived, §9.2: the caller ACKs and BYEs),
/// `answered_before_cancel` (the 200 reached her before her CANCEL left) or
/// `abandoned` (the CANCEL won, 487). On this paused fake net the timing is
/// fixed: the caller CANCELs one ring after her 180, the callee answers one
/// ring after sending it, so the 200 always leaves first and every call is the
/// crossing. The other two branches through the SUT are pinned by
/// `realcall_abandon_ringing_ending_follows_the_answer_timing` (b2bua-harness).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_cancel_answer_crossing_accepts_either_branch() {
    let (h, b2bua, core, transport) = setup_fake(7070).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    // A non-zero ring gives the CANCEL a window to race the 200.
    let mut c = cfg(b2bua.addr, 8.0, 2, 8, 0xCA5C);
    c.call.ring_delay = Duration::from_millis(30);
    let driver =
        Driver::new(c, vec![mix("cancel_answer_crossing", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 5, "governor under-delivered: {total}");
    assert_eq!(reporter.count("cancel_answer_crossing", &ResultClass::Ok), total, "{prom}");
    let crossed = format!(
        "loadgen_branch_total{{stream=\"main\",shape=\"cancel_answer_crossing\",branch=\"answered_despite_cancel\"}} {total}"
    );
    assert!(prom.lines().any(|l| l == crossed), "expected `{crossed}` in:\n{prom}");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    // A cancel-wins straggler may lean on the SUT's own dead-call detection —
    // advance past it before the strict release oracle.
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C5: early UPDATE through the SUT — a reliable (100rel) establishment where
/// the caller renegotiates media on the EARLY dialog (after the PRACK, before
/// the final 200; RFC 3311 §5.1). Every call establishes, tears down and
/// settles OK under the strict per-call audit, with a fully-reaped SUT.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_prack_update_early() {
    let (_h, b2bua, core, transport) = setup_fake(7080).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 10.0, 2, 8, 0xE4E1),
        vec![mix("prack_update_early", 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("prack_update_early", &ResultClass::Ok);
    assert!(total >= 8, "governor under-delivered: {total}");
    assert_eq!(ok, total, "an early-UPDATE call was NOK:\n{}", reporter.render_prometheus());
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the fake net"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C3 (b): drop BOTH crossing BYEs (the caller's AND the callee's — `leg: None`
/// arms every endpoint's first outbound BYE) and let auto-retransmit heal them.
/// The Timer-E resend on each side recovers the loss, the ledger closes, and
/// every call is OK with a clean audit — proving the crossing teardown is
/// loss-resilient in both directions (`permanent: false`).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_crossing_bye_recovers_dropped_byes() {
    let (_h, b2bua, core, transport) = setup_fake(7060).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 2.0, 2, 4, 0xB7EE);
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
    let driver = Driver::new(c, vec![mix("crossing_bye", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("crossing_bye", &ResultClass::Ok);
    assert!(total >= 2, "no calls ran");
    assert!(drops >= total, "the targeted BYE drop never fired: drops={drops} calls={total}");
    assert_eq!(
        ok,
        total,
        "a dropped crossing BYE was not recovered by retransmit:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("crossing_bye", &ResultClass::RfcAuditFail),
        0,
        "a recovered drop leaked into the RFC audit:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C6 (a): ten SERIALIZED re-INVITE cycles per call (the "10 re-INVITEs" ask)
/// through the real driver over the fake net. Each cycle is gated on the prior
/// one COMPLETING (`reneg_count`), so no two re-INVITEs are ever in flight (no
/// glare); every call reaches all ten renegotiations, tears down, and settles
/// OK under the strict per-call audit — deterministically on virtual time.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
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
    assert!(total >= 8, "governor under-delivered: {total}");
    assert_eq!(
        ok,
        total,
        "a serialized ×10 re-INVITE call was NOK:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the fake net"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
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
#[ignore = "slow lane: loadgen"]
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
    assert!(total >= 5, "too few calls: {total}");
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
    settle_until(|| b2bua.is_reaped()).await;
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
#[ignore = "slow lane: loadgen"]
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
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// C1/E3 — TRUE forking end-to-end through the transparent-relay SUT. bob emits
/// three distinct-To-tag 18x on its ONE INVITE server txn (as if a downstream
/// proxy forked); the transparent CORE relay (`setup_fake`'s `route_all_with_
/// refer` leaves `relay_first_18x_to_180 = None`) forwards each as its own
/// a-facing early dialog; the fork-aware caller (C1b) tracks the early-dialog
/// set and confirms on the winning fork's 2xx (§13.2.2.4). Every call OK, clean
/// audit, no orphans (the C1d mux fix keeps the distinct-tag 18x from being
/// deduped), reaped. `run` accepts each shape id so all three fork variants
/// share this body.
async fn fork_e2e(base: u16, shape: &'static str, seed: u64) {
    let (_h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 12.0, 2, 8, seed),
        vec![mix(shape, 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count(shape, &ResultClass::Ok);
    assert!(total >= 8, "governor under-delivered forked calls: {total}");
    assert_eq!(ok, total, "a forked call was NOK:\n{}", reporter.render_prometheus());
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on a forked call (distinct-tag 18x mis-demuxed?):\n{}",
        core.stats().samples().join("\n")
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_forked_plain() {
    fork_e2e(7070, "forked", 0xF04C).await;
}

// `forked_loser_late_200` has NO through-SUT test: a terminating B2BUA absorbs
// the loser's late 2xx, so the caller's ACK+BYE-the-loser path is unreachable
// through a SUT. It is pinned SUT-less in `scenario_harness::actor::tests`.

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_forked_reliable() {
    fork_e2e(7090, "forked_reliable", 0xF06A).await;
}

/// A forked call answered by a fork that never rang (a 2xx To-tag no 18x
/// carried, RFC 3261 §13.2.2.4), relayed through the SUT.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_forked_unrung() {
    fork_e2e(7160, "forked_unrung", 0xF07B).await;
}

/// C1/E3 loss resilience: the forked establishment (three distinct-tag 18x +
/// the winner's 2xx, all relayed through the SUT) under ~8%/datagram loss +
/// auto-retransmit. A dropped best-effort 18x just means the caller observes
/// one fewer early dialog (the winner still confirms); a dropped winner 2xx /
/// ACK / BYE is recovered by the retransmit engine. A loss-model drop must
/// never surface as an RFC-audit finding (the fork-slice replication keeps
/// non-first forks correctly judged), and every NOK is a bounded timeout.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_forked_loss_soak() {
    let (h, b2bua, core, transport) = setup_fake(7100).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 10.0, 3, 8, 0xF0A2);
    c.default_tuning = CallTuning { drop_rate: 0.08, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("forked", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed) + core.stats().dropped_in.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("forked", &ResultClass::Ok);
    let timeouts = reporter.count("forked", &ResultClass::Timeout);
    assert!(drops > 0, "loss model dropped nothing — the soak is vacuous");
    assert!(total >= 20, "too few forked calls: {total}");
    assert!(ok > 0, "no forked call recovered under loss:\n{}", reporter.render_prometheus());
    assert_eq!(
        reporter.count("forked", &ResultClass::RfcAuditFail),
        0,
        "a loss-model drop surfaced as an RFC-audit finding on a fork:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        ok + timeouts,
        total,
        "a forked call failed with a non-timeout class under loss:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss");
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

// ── loss-soak matrix + per-new-element targeted drops ───────────────────────

/// A standard paused-clock probabilistic loss soak for a through-SUT shape `id`:
/// ~7%/datagram loss + auto-retransmit, then the graceful-degradation gates —
/// retransmit recovers SOME calls, no drop surfaces as an RFC-audit finding,
/// every NOK degrades to a bounded `timeout`, and after draining past the SUT's
/// 32 s dead-call detection the strict release oracle holds. `ring_ms` gives a
/// non-zero ring where the shape needs a race window (0 otherwise).
async fn loss_soak(base: u16, id: &'static str, seed: u64, ring_ms: u64) {
    let (h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 10.0, 2, 8, seed);
    c.call.ring_delay = Duration::from_millis(ring_ms);
    c.default_tuning = CallTuning { drop_rate: 0.07, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix(id, 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed) + core.stats().dropped_in.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count(id, &ResultClass::Ok);
    let timeouts = reporter.count(id, &ResultClass::Timeout);
    assert!(drops > 0, "{id}: loss model dropped nothing — the soak is vacuous");
    assert!(total >= 5, "{id}: too few calls: {total}");
    assert!(
        ok > 0,
        "{id}: retransmit recovered NO call under loss: ok={ok} timeouts={timeouts} drops={drops}\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count(id, &ResultClass::RfcAuditFail),
        0,
        "{id}: a loss-model drop surfaced as an RFC-audit finding:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        ok + timeouts,
        total,
        "{id}: a call failed with a non-timeout class under loss:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "{id}: mux registry leak under loss");
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// D2 loss soak: the C5 early-UPDATE shape under loss (the reliable 183, PRACK,
/// early UPDATE and final 200 all subject to drop + retransmit recovery).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_prack_update_early_loss_soak() {
    loss_soak(7090, "prack_update_early", 0xE4E2, 0).await;
}

/// D2 loss soak: a GENERATED matrix cell (forked establishment + re-INVITE)
/// through the SUT under loss — proves the composed cross-product cell is
/// loss-resilient, not just the hand-written shapes.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_forked_reinvite_loss_soak() {
    loss_soak(7100, "forked+reinvite", 0xF012, 0).await;
}

/// D2 loss soak: a GENERATED reliable+re-INVITE cell under loss.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_reliable_reinvite_loss_soak() {
    loss_soak(7110, "reliable+reinvite", 0x5EA1, 0).await;
}

/// D2 targeted drop (recovery) — the C5 EARLY UPDATE: discard each call's first
/// outbound UPDATE before the wire; the auto-retransmit Timer-E resend heals it,
/// the callee releases its held INVITE 200, and every call is OK with a clean
/// audit (`permanent: false`).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_recovers_dropped_early_update() {
    let (_h, b2bua, core, transport) = setup_fake(7120).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 2.0, 2, 4, 0x0DDE);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop {
            method: "UPDATE",
            nth: 1,
            permanent: false,
            dir: DropDir::Outbound,
            leg: None,
        }),
    };
    let driver = Driver::new(c, vec![mix("prack_update_early", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("prack_update_early", &ResultClass::Ok);
    assert!(total >= 2, "no calls ran");
    assert!(drops >= total, "the targeted UPDATE drop never fired: drops={drops} calls={total}");
    assert_eq!(
        ok,
        total,
        "a dropped early UPDATE was not recovered by retransmit:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(reporter.count("prack_update_early", &ResultClass::RfcAuditFail), 0);

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// NO-ANSWER-triggered failover end-to-end on the paused clock:
///
///   INVITE(routes plan, per-route `no_answer_ms`) → bob 180 …
///   silence … → the SUT's OWN NoAnswer timer fires → CANCEL bob (487, the
///   leg settles via the hop-ACK) → /call/failure walks the plan → bob2
///   answers → talk → BYE.
///
/// Every call OK under the strict per-call audit (the SUT-timer-driven CANCEL
/// of the silent primary is the EXPECTED teardown of that leg), zero orphans
/// (the rerouted `sip:bob2@…` leg demuxes via the R-URI-user picker), no
/// mux/SUT leak. The ~2 s ring dwell per call is free on virtual time.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_rerouting_noanswer() {
    let (h, b2bua, core, transport) = setup_fake_api_call(7140).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 2 }));

    // The pinned layout: the egress realizes [bob, bob2] as the X-Api-Call
    // routes failover plan, each route arming the SUT's no-answer ring timer.
    let mut c = cfg(b2bua.addr, 6.0, 2, 8, 0x0A47);
    c.call.egress = EgressPolicy::ApiCallPin;

    let driver = Driver::new(c, vec![mix("rerouting_noanswer", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("rerouting_noanswer", &ResultClass::Ok);
    assert!(total >= 5, "governor under-delivered: {total}");
    assert_eq!(ok, total, "a no-answer reroute call was NOK:\n{}", reporter.render_prometheus());
    assert_eq!(
        reporter.count("rerouting_noanswer", &ResultClass::RfcAuditFail),
        0,
        "the SUT-timer-driven CANCEL surfaced as an RFC-audit finding:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        core.stats().orphan_no_header.load(Relaxed)
            + core.stats().orphan_unknown_token.load(Relaxed)
            + core.stats().orphan_stray.load(Relaxed),
        0,
        "orphans on the no-answer reroute (failover-leg demux gap?)"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (rerouting_noanswer)");
    // Drain past the SUT's dead-call detection before the strict release oracle
    // (a straggler CANCEL/487 tail settles on the paused timeline either way).
    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// D2 targeted drop (recovery) — the per-fork PRACK (C1b): discard each call's
/// first outbound PRACK; the UAS keeps retransmitting its reliable 183 until the
/// PRACK's Timer-E resend lands, then answers. Every forked-reliable call is OK
/// with a clean audit (`permanent: false`).
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn loadgen_fake_net_recovers_dropped_prack() {
    let (_h, b2bua, core, transport) = setup_fake(7130).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 2.0, 2, 4, 0x9ACA);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop {
            method: "PRACK",
            nth: 1,
            permanent: false,
            dir: DropDir::Outbound,
            leg: None,
        }),
    };
    let driver = Driver::new(c, vec![mix("forked_reliable", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("forked_reliable", &ResultClass::Ok);
    assert!(total >= 2, "no calls ran");
    assert!(drops >= total, "the targeted PRACK drop never fired: drops={drops} calls={total}");
    assert_eq!(
        ok,
        total,
        "a dropped PRACK was not recovered by retransmit:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(reporter.count("forked_reliable", &ResultClass::RfcAuditFail), 0);

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
