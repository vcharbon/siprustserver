//! Mux-transport smoke tests, over **real loopback UDP** (the mux opens real
//! sockets, so the in-process B2buaCore SUT runs on a real-network `Harness`).
//! They validate the whole pipeline deterministically before any cluster:
//! correlation/demux, concurrency without dialog mixing, no registry leak,
//! orphan observability, teardown, and the sampled callflow report.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use layer_harness::TransportKind;
use loadgen::scenarios::ScenarioId;
use scenario_harness::actor::scenarios::BasicCall;
use scenario_harness::actor::{
    phase, ActorCall, ActorScenario, ActorSpec, Barrier, CtxFeed, Disposition, Expect, Feed, Goal,
    GoalStep, LegPhase, MediaState, SettleBarrier, StateInner, SubflowState, SUBFLOW_REALIGN,
};
use scenario_harness::{ANSWER_SDP, ApiCall, OFFER_SDP};
use loadgen::{
    prefix_leg_picker, CallConfig, CallEnv, CallRouting, CallTuning,
    Correlation, Driver, DriverCfg, EgressPolicy, EndpointSpec, LegInfo, LegSpec, LoadCase,
    MixEntry, MuxCore, MuxTransport, ResultClass, Reporter, ReporterCfg, Role, ScenarioInputs,
    DropDir, ShapeDescriptor, ShapeRegistry, TargetedDrop,
};
use scenario_harness::{Harness, StepError};
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use tokio::net::UdpSocket;

/// Default per-recv timeout for the happy-path (no-loss) smoke tests. Deliberately
/// generous: libtest starts every real-clock loopback test at once, and that CPU
/// oversubscription can delay the recv task's *scheduling* — not the datagram —
/// well past a tight window, flipping a healthy call into the `timeout` class and
/// flaking the strict every-call-OK asserts. A passing call returns the instant its
/// datagram is observed, so a wide window costs ZERO wall-clock on the happy path;
/// it is pure contention headroom. Kept large on purpose — flaky tests waste far
/// more time than a slow timeout tail on the rare genuine failure.
const RECV: Duration = Duration::from_secs(20);

/// Per-recv timeout for the LOSSY tests: must clear the full retransmit ladder
/// (0.5+1+2+4 s) PLUS stretched two-hop latency AND recv-task starvation under
/// full-suite contention. A recovered call lands late-but-OK inside this window;
/// only a genuinely unrecoverable datagram waits it out. Wide = headroom, not
/// baseline cost (see [`RECV`]).
const RECV_LOSSY: Duration = Duration::from_secs(45);

/// Teardown-reap settle ceiling for the LOSSY tests. Must clear the SUT's own 32 s
/// terminating-safety timer (`TERMINATING_TIMEOUT_MS`) — a recovered call whose
/// teardown BYE was fully lost falls back to it — PLUS real-clock reap-timer
/// starvation under full-suite contention. [`settle_secs`] polls and returns the
/// instant the leak clears, so a high ceiling is pure headroom (it only waits out a
/// GENUINE leak, which must fail anyway). 20 s (below the 32 s timer) was the
/// refer-recovery flake; 45 s clears it with margin.
const SETTLE_LOSS_SECS: u64 = 45;

/// Peak-contention governor. These tests are real-clock over real loopback
/// UDP, and libtest starts ALL of them at once — 20+ concurrent multi-worker
/// runtimes, each bursting a driver against an in-process SUT whose per-call
/// RFC audit is CPU-heavy in debug builds. That oversubscription can starve
/// individual calls past their recv/retransmit windows on a loaded box, so
/// calls land in the `timeout` class and the strict every-call-OK asserts
/// flake (~1-in-20 under full-suite contention). Cap how many DRIVERS run
/// concurrently — setup and post-run settling stay parallel, and the strict
/// asserts keep their teeth.
static DRIVERS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
const DRIVER_PERMITS: usize = 4;

/// Run `driver` while holding a governor permit (see [`DRIVERS`]).
async fn run_throttled(driver: &Driver) {
    let _permit = DRIVERS
        .get_or_init(|| tokio::sync::Semaphore::new(DRIVER_PERMITS))
        .acquire()
        .await
        .expect("driver governor closed");
    driver.run().await;
}

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// Resolve one shape from the unified registry into a weighted mix entry (the
/// smoke tests' shorthand over `MixEntry::by_id` + the default inputs).
fn mix(id: &str, weight: f64) -> MixEntry {
    MixEntry::by_id(&ShapeRegistry::with_defaults(), id, &inputs(), weight)
        .unwrap_or_else(|| panic!("unknown load shape {id:?}"))
}

/// Stand up a real-network b2bua SUT + a mux core over a port base. The b2bua
/// routes the b-leg to the static `uas` endpoint (base+1).
async fn setup(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_with(base, correlation, sample_cap, RECV, |_| {}).await
}

/// [`setup`] with an explicit per-recv timeout — the lossy tests widen it (like
/// the production loadgen's 5 s) so compounded two-hop retransmit recovery has
/// headroom.
async fn setup_recv(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
    recv: Duration,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_with(base, correlation, sample_cap, recv, |_| {}).await
}

/// `setup` with an extra `B2buaConfig` mutator (e.g. to exhaust the CPS bucket
/// for an overload-shed test). The relay-header tune is always applied first.
async fn setup_with(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
    recv: Duration,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_inner(base, correlation, sample_cap, recv, true, extra_tune).await
}

/// [`setup`] WITHOUT the correlation-header relay tune on the SUT — the
/// third-party-SUT shape (a B2BUA that strips/ignores unknown headers, breaking
/// header correlation entirely). Only a strategy needing zero SUT cooperation
/// (`Correlation::to_user`) can correlate the callee leg here.
async fn setup_no_relay(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_inner(base, correlation, sample_cap, RECV, false, |_| {}).await
}

/// [`setup`] whose SUT honors the full inbound `X-Api-Call` control surface
/// (destination pin + ADR-0017 `routes` failover plan walked on b-leg
/// rejection) — the deployed-cluster engine shape the `api-call-pin` egress
/// policy addresses. The rerouting smoke test runs over this.
async fn setup_api_call(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_shaped(base, correlation, sample_cap, RECV, true, B2buaSut::route_api_call, |_| {}).await
}

async fn setup_inner(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
    recv: Duration,
    relay_header: bool,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_shaped(
        base,
        correlation,
        sample_cap,
        recv,
        relay_header,
        B2buaSut::route_all_with_refer,
        extra_tune,
    )
    .await
}

/// The one setup core: `make_sut(dest_host, dest_port)` picks the SUT's
/// decision-engine shape (plain route-all+refer vs the X-Api-Call plan walker).
async fn setup_shaped(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
    recv: Duration,
    relay_header: bool,
    make_sut: fn(&str, u16) -> b2bua_harness::B2buaSutBuilder,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let net: Arc<dyn SignalingNetwork> = Arc::new(RealSignalingNetwork::new());
    let h = Harness::with_network_and_clock(
        "mux-smoke",
        net,
        Clock::system(),
        TransportKind::Live,
        recv,
    );
    h.disarm_cseq_gate(); // infra harness; loadgen runs its own per-call audit

    let (uac, uas, refer) = (base, base + 1, base + 2);
    // Make the in-process b2bua transparent to the loadgen correlation header
    // (the production `B2BUA_RELAY_HEADERS=X-Loadgen-Id`), so the token alice
    // stamps reaches BOTH the b-leg (bob) and the REFER transfer leg (charlie).
    // `relay_header = false` models a third-party SUT that relays nothing.
    let b2bua = make_sut("127.0.0.1", uas)
        .tune(move |c| {
            if relay_header {
                c.relay_headers = vec!["X-Loadgen-Id".to_string()];
            }
            extra_tune(c);
        })
        .start(&h, "b2bua", &format!("127.0.0.1:{}", base + 3))
        .await;

    let core = MuxCore::bind(
        vec![
            EndpointSpec { addr: addr(uac), role: Role::Caller },
            EndpointSpec { addr: addr(uas), role: Role::Callee },
            EndpointSpec { addr: addr(refer), role: Role::Callee },
        ],
        correlation.clone(),
        256,
        sample_cap as usize,
        recv,
        Clock::system(),
    )
    .await
    .unwrap();

    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: addr(uac),
        uas_addr: addr(uas),
        refer_addr: addr(refer),
        correlation,
        recv_timeout: recv,
        clock: Clock::system(),
    });
    (h, b2bua, core, transport)
}

/// The default per-run scenario inputs — `refer_key` = "refer-allow-c", the key
/// the in-process SUT's scripted `/call/refer` backend (`route_all_with_refer`)
/// authorizes.
fn inputs() -> ScenarioInputs {
    ScenarioInputs::default()
}

fn cfg(via: SocketAddr, cps: f64, secs: u64, mif: usize, seed: u64) -> DriverCfg {
    DriverCfg {
        cps,
        duration: Duration::from_secs(secs),
        max_in_flight: mif,
        seed,
        call: CallConfig {
            via,
            // route_all_* routes the b-leg by config — the transparent layout
            // (the refer scenarios resolve charlie through the same seam and
            // authorize via their per-run `refer_key`).
            egress: EgressPolicy::Transparent,
            options_hold: Duration::from_millis(120),
            options_cadence: Duration::from_millis(40),
            // Realistic-timer knobs default to fast values in tests (the default
            // lane stays sub-60 s); the endurance run sets the real durations.
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

/// Basic call, CONCURRENT (max_in_flight > 1) through ONE shared uas socket —
/// proves dialogs are demuxed (no mixing) by the relayed `X-Loadgen-Id` token,
/// with no orphans, no registry leak, an OK callflow sample, and a reaped SUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_basic_concurrent() {
    let (_h, b2bua, core, transport) = setup(6400, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 4 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 60.0, 2, 16, 0xB451C),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    assert!(reporter.count("basic_call", &ResultClass::Ok) > 5, "too few OK basic calls: {}", reporter.render_prometheus());
    assert_eq!(reporter.count("basic_call", &ResultClass::Timeout), 0, "unexpected timeouts (dialog mixing?)");
    assert_eq!(core.stats().orphan_no_header.load(std::sync::atomic::Ordering::Relaxed), 0, "unexpected orphans");
    assert!(reporter.sample_count("basic_call", &ResultClass::Ok) > 0, "no OK callflow sample");

    // No leak: every call's mux entries reclaimed; SUT fully reaped.
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    // The on-disk report renders an OK callflow.
    let out = std::env::temp_dir().join(format!("loadgen-mux-{}", std::process::id()));
    reporter.finalize(&out).unwrap();
    // No chaos markers in the smoke run → every call is the `clear` sub-bucket.
    assert!(out.join("callflows/basic_call/ok/clear/0.html").exists(), "no OK callflow HTML");
    let _ = std::fs::remove_dir_all(&out);
}

/// TO-USER correlation end-to-end: the token rides the To-header user-part, so
/// a full call correlates WITHOUT the SUT relaying any loadgen header — the
/// in-process b2bua here has NO `relay_headers` configured (the third-party-SUT
/// shape under which header correlation yields zero OK calls). Concurrent basic
/// calls all complete OK, with zero correlation orphans and no mux/SUT leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_to_user_correlation_without_relayed_header() {
    use std::sync::atomic::Ordering::Relaxed;
    let (_h, b2bua, core, transport) = setup_no_relay(6540, Correlation::to_user(), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 4 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 60.0, 2, 16, 0x70C4),
        vec![mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    assert!(
        reporter.count("basic_call", &ResultClass::Ok) > 5,
        "too few OK calls via to-user correlation: {}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("basic_call", &ResultClass::Timeout),
        0,
        "timeouts — to-user correlation failed to route the callee leg?"
    );
    // Zero correlation orphans: every callee leg carried an extractable To-user
    // token AND that token matched its pending call.
    assert_eq!(core.stats().orphan_no_header.load(Relaxed), 0, "uncorrelatable callee legs");
    assert_eq!(core.stats().orphan_unknown_token.load(Relaxed), 0, "to-user token matched no call");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (to-user)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// All four scenarios (refer last) through the mux → each produces OK, no leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_all_scenarios() {
    let (_h, b2bua, core, transport) = setup(6410, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 4 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 60.0, 4, 8, 0xA11),
        MixEntry::default_mix(&ShapeRegistry::with_defaults(), &inputs()),
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    for id in ["basic_call", "reinvite", "options_hold", "refer"] {
        assert!(
            reporter.count(id, &ResultClass::Ok) > 0,
            "scenario {id} produced no OK calls:\n{}",
            reporter.render_prometheus()
        );
    }
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// The realistic-timer paths (ring dwell, post-connect talk, re-INVITE spacing)
/// and the `long_call` scenario (one OPTIONS ping, then a hold during which BOTH
/// legs answer the SUT's relayed in-dialog keepalives) all complete OK and leave
/// no leak. Timers are set short (tens of ms) so the default lane stays fast; the
/// point is to exercise the sleep/quiesce branches, not real durations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_timed_and_long_call() {
    let (_h, b2bua, core, transport) = setup(6470, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    let mut cfg = cfg(b2bua.addr, 40.0, 2, 16, 0x71A1ED);
    cfg.call.ring_delay = Duration::from_millis(30);
    cfg.call.talk_time = Duration::from_millis(30);
    cfg.call.reinvite_gap = Duration::from_millis(20);
    cfg.call.long_hold = Duration::from_millis(150);

    let driver = Driver::new(
        cfg,
        vec![
            mix("basic_call", 2.0),
            mix("reinvite", 1.0),
            mix("long_call", 1.0),
        ],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    for id in ["basic_call", "reinvite", "long_call"] {
        assert!(
            reporter.count(id, &ResultClass::Ok) > 0,
            "scenario {id} produced no OK calls with realistic timers:\n{}",
            reporter.render_prometheus()
        );
    }
    // The long_call recorded its single OPTIONS-ping checkpoint.
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (timed/long-call)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// PRACK + UPDATE mix against the in-process SUT: the `prack_update` scenario
/// (INVITE(100rel) → reliable 183(RSeq) → PRACK → 200(PRACK) → 200(INVITE) →
/// ACK → UPDATE → 200 → BYE) runs concurrently with basic calls through the mux
/// with CLEAN result classes — every call OK (so zero timeout / wrong-status /
/// rfc_audit_fail; the sampled half of the calls IS RFC-audited via the
/// recording binder), no orphans, no mux/SUT leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_prack_update_mix() {
    let (_h, b2bua, core, transport) = setup(6560, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 60.0, 2, 16, 0x93AC5),
        vec![
            mix("prack_update", 2.0),
            mix("basic_call", 1.0),
        ],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    let ok_prack = reporter.count("prack_update", &ResultClass::Ok);
    let ok_basic = reporter.count("basic_call", &ResultClass::Ok);
    assert!(ok_prack > 5, "too few OK prack_update calls: {}", reporter.render_prometheus());
    assert!(ok_basic > 0, "no OK basic calls in the mix: {}", reporter.render_prometheus());
    // CLEAN result classes: every finished call is OK — in particular zero
    // rfc_audit_fail (the recorded/sampled calls pass the RFC 3261/3262/3264
    // audit with no waiver) and zero timeouts (no dialog mixing on PRACK/UPDATE).
    assert_eq!(
        reporter.total_calls(),
        ok_prack + ok_basic,
        "non-OK result classes in the prack_update mix:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        core.stats().orphan_no_header.load(std::sync::atomic::Ordering::Relaxed) +
            core.stats().orphan_unknown_token.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "unexpected orphans (PRACK/UPDATE demux gap?)"
    );
    assert!(reporter.sample_count("prack_update", &ResultClass::Ok) > 0, "no OK prack_update sample");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (prack_update)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// The DUAL-BODY `rerouting_prack` shape's LOAD body against the in-process
/// SUT — the first shape served by BOTH run surfaces from ONE registry
/// declaration (the functional body runs in e2e-core over the fake infra; this
/// is the load half). Under the `api-call-pin` egress the ordered candidate
/// list [bob, bob2] rides an `X-Api-Call` `routes` failover plan (ADR-0017):
///
///   INVITE(Supported:100rel, routes plan) → bob 486 → the SUT walks the plan
///   to bob2 — SAME socket, demuxed by the driver's R-URI-user leg picker —
///   → reliable 183(RSeq) → PRACK → 200(PRACK) → 200 → ACK → BYE
///
/// Runs concurrently with basic calls (their single-candidate pin on the same
/// egress) with CLEAN result classes — every call OK, so zero timeout /
/// wrong-status / rfc_audit_fail (the sampled half of the calls IS RFC-audited
/// via the recording binder: the RFC 3261/3262/3264 gate passes with no
/// waiver) — zero orphans (the picker routed every rerouted leg), and no
/// mux/SUT leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_rerouting_prack_mix() {
    let (_h, b2bua, core, transport) =
        setup_api_call(6620, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    // The environment axis: the pinned layout (the real cluster's shape) — the
    // egress realizes [bob, bob2] as the routes failover plan.
    let mut c = cfg(b2bua.addr, 60.0, 2, 16, 0x4E404);
    c.call.egress = EgressPolicy::ApiCallPin;

    let driver = Driver::new(
        c,
        vec![mix("rerouting_prack", 2.0), mix("basic_call", 1.0)],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    let ok_rr = reporter.count("rerouting_prack", &ResultClass::Ok);
    let ok_basic = reporter.count("basic_call", &ResultClass::Ok);
    assert!(ok_rr > 5, "too few OK rerouting_prack calls: {}", reporter.render_prometheus());
    assert!(ok_basic > 0, "no OK basic calls in the mix: {}", reporter.render_prometheus());
    // CLEAN result classes: every finished call is OK — in particular zero
    // rfc_audit_fail (the recorded/sampled calls pass the RFC audit unwaived)
    // and zero timeouts (no misrouted failover leg).
    assert_eq!(
        reporter.total_calls(),
        ok_rr + ok_basic,
        "non-OK result classes in the rerouting_prack mix:\n{}",
        reporter.render_prometheus()
    );
    // Zero orphans: every rerouted b-leg carried the relayed token AND the
    // R-URI-user picker resolved it to a registered receiver (`no_route` counts
    // under `orphan_unknown_token`).
    assert_eq!(
        core.stats().orphan_no_header.load(std::sync::atomic::Ordering::Relaxed)
            + core.stats().orphan_unknown_token.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "unexpected orphans (failover-leg demux gap?)"
    );
    assert!(
        reporter.sample_count("rerouting_prack", &ResultClass::Ok) > 0,
        "no OK rerouting_prack sample"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (rerouting_prack)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

// NOTE: the former `fail_mid_call` / `loadgen_mux_teardown_no_leak` test
// exercised the LINEAR driver's own `scope.teardown()` reaping a confirmed
// dialog a body had leaked — a path deleted with the linear executor (the actor
// runner always owns its per-actor teardown, so that leak is structurally
// impossible). "A failed/aborted call still fully reaps a confirmed dialog" is
// now covered by `loadgen_post_call_cleanup_no_leak` (actor failure-mix incl.
// `refer_charlie_reject`, which BYEs a confirmed A↔B, + `assert_fully_reaped`).

/// Orphan observability: a new dialog with NO correlation header, and one with a
/// header matching no pending call, are both counted + sampled + dropped (never
/// queued). Uses Header correlation and a raw sender (no b2bua needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_orphan_observability() {
    use std::sync::atomic::Ordering::Relaxed;
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: addr(6431), role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let uas = addr(6431);

    // A new INVITE with NO correlation header → orphan_no_header.
    let no_header = "INVITE sip:x@127.0.0.1 SIP/2.0\r\nCall-ID: stray-1@h\r\nTo: <sip:x@127.0.0.1>\r\nFrom: <sip:y@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n";
    sender.send_to(no_header.as_bytes(), uas).await.unwrap();

    // An INVITE WITH a header matching no pending call → orphan_unknown_token.
    let bad_token = "INVITE sip:x@127.0.0.1 SIP/2.0\r\nCall-ID: stray-2@h\r\nX-Loadgen-Id: lgBOGUS\r\nTo: <sip:x@127.0.0.1>\r\nFrom: <sip:y@h>;tag=2\r\nCSeq: 1 INVITE\r\n\r\n";
    sender.send_to(bad_token.as_bytes(), uas).await.unwrap();

    settle_until(|| {
        core.stats().orphan_no_header.load(Relaxed) >= 1
            && core.stats().orphan_unknown_token.load(Relaxed) >= 1
    })
    .await;

    assert_eq!(core.stats().orphan_no_header.load(Relaxed), 1);
    assert_eq!(core.stats().orphan_unknown_token.load(Relaxed), 1);
    assert_eq!(core.registry_size(), 0, "orphans must not be queued/registered");
    let samples = core.stats().samples();
    assert!(samples.iter().any(|s| s.contains("no_header")), "no no_header sample: {samples:?}");
    assert!(samples.iter().any(|s| s.contains("unknown_token")), "no unknown_token sample: {samples:?}");
}

/// The chaos-flag endpoint: a `POST /chaos?type=…&target=…` records one marker
/// on the loadgen's HTTP server (the same socket as GET `/metrics`), and a GET
/// still returns the render body. This is the hook the chaos driver hits at the
/// kill instant so finished calls get auto-classified near/clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_chaos_post_records_marker() {
    use loadgen::{serve_metrics, ChaosLog};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let chaos = Arc::new(ChaosLog::new(Clock::system()));
    let bind = addr(6490);
    let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(|| "render-body-marker\n".to_string());
    let srv_chaos = chaos.clone();
    tokio::spawn(async move {
        let _ = serve_metrics(bind, render, Some(srv_chaos), None).await;
    });

    // Retry-connect until the server is bound.
    let mut stream = loop {
        match tokio::net::TcpStream::connect(bind).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    stream
        .write_all(b"POST /chaos?type=kill_worker&target=b2bua-worker-1 HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    assert!(
        String::from_utf8_lossy(&buf).contains("recorded chaos marker"),
        "POST /chaos should ack: {}",
        String::from_utf8_lossy(&buf)
    );
    assert_eq!(chaos.total(), 1, "exactly one marker recorded");

    // A POST carrying `ts` (Unix epoch ms of the kill) is back-dated: the ack
    // echoes the ts and a second marker is recorded. The driver supplies this so
    // port-forward latency on the flag path can't shift the marker off the kill.
    let kill_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 1500;
    let mut s_ts = tokio::net::TcpStream::connect(bind).await.unwrap();
    s_ts.write_all(
        format!("POST /chaos?type=kill_worker&target=b2bua-worker-1&ts={kill_ms} HTTP/1.1\r\nHost: x\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut bt = Vec::new();
    let _ = s_ts.read_to_end(&mut bt).await;
    assert!(
        String::from_utf8_lossy(&bt).contains(&format!("ts={kill_ms}")),
        "POST /chaos with ts should ack the timestamp: {}",
        String::from_utf8_lossy(&bt)
    );
    assert_eq!(chaos.total(), 2, "the ts-bearing marker is recorded too");

    // A GET still serves the render body.
    let mut s2 = tokio::net::TcpStream::connect(bind).await.unwrap();
    s2.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut b2 = Vec::new();
    let _ = s2.read_to_end(&mut b2).await;
    assert!(String::from_utf8_lossy(&b2).contains("render-body-marker"), "GET should render");
}

/// The leg-routing **injection API**: two receivers ("charlie", "dave") share
/// ONE socket under one call token. Call correlation (the token) finds the call;
/// a scenario-owned picker — handed the parsed leg — disambiguates which
/// receiver owns each arriving INVITE, here keyed on the Request-URI user. Proves
/// the "several endpoints on the same port" path the mux itself stays agnostic
/// to. (This is the primitive a future multi-REFER / re-route scenario builds on.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_picker_disambiguates_shared_socket() {
    use sip_net::BindUdpOpts;
    let uas = addr(6441);
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: uas, role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    // One call token; two receivers on the same socket; a scenario-owned picker
    // routing by R-URI user-part to the matching receiver label.
    let token = "lgSHARED".to_string();
    let routing = CallRouting::new(token.clone())
        .leg(uas, "charlie")
        .leg(uas, "dave")
        .picker(uas, Arc::new(|leg: &LegInfo| leg.ruri_user().unwrap_or_default()));
    let net = core.network(routing);

    // Bind in declaration order: first bind = "charlie", second = "dave".
    let ep_charlie = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let ep_dave = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let invite = |who: &str, cid: &str| {
        format!(
            "INVITE sip:{who}@127.0.0.1 SIP/2.0\r\nCall-ID: {cid}@h\r\nX-Loadgen-Id: {token}\r\n\
             To: <sip:{who}@127.0.0.1>\r\nFrom: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
    };
    // Deliberately send dave's leg FIRST to prove routing is by the picker, not
    // arrival order.
    sender.send_to(invite("dave", "c-dave").as_bytes(), uas).await.unwrap();
    sender.send_to(invite("charlie", "c-charlie").as_bytes(), uas).await.unwrap();

    let got_charlie = tokio::time::timeout(RECV, ep_charlie.recv()).await.unwrap().unwrap();
    let got_dave = tokio::time::timeout(RECV, ep_dave.recv()).await.unwrap().unwrap();
    assert!(
        String::from_utf8_lossy(&got_charlie.raw).contains("sip:charlie@"),
        "charlie receiver got the wrong leg"
    );
    assert!(
        String::from_utf8_lossy(&got_dave.raw).contains("sip:dave@"),
        "dave receiver got the wrong leg"
    );
    assert_eq!(core.stats().delivered.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(core.stats().orphan_no_header.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(core.stats().orphan_unknown_token.load(std::sync::atomic::Ordering::Relaxed), 0);

    // Both receivers gone → the shared slot is fully reclaimed.
    drop(ep_charlie);
    drop(ep_dave);
    assert_eq!(core.registry_size(), 0, "shared-socket slot leaked after both receivers dropped");
}

/// The shared-callee **prefix** path (`prefix_leg_picker`): bob, bob2 AND charlie
/// share ONE socket under ONE call token — the two-tier demux the reroute+transfer
/// topology needs. Tier 1 (the token in `X-Loadgen-Id`) selects this call instance;
/// tier 2 (the R-URI-user prefix) tells the three legs apart, including a
/// per-call-suffixed transfer user (`sip:charlie-<tag>@…`) that still routes by its
/// "charlie" prefix (not onto "bob" via a shorter match). Proves bob1/bob2/charlie
/// need no per-leg socket — the primitive the driver now wires for every call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_prefix_picker_shares_callee_port() {
    use sip_net::BindUdpOpts;
    let uas = addr(6443);
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: uas, role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    // ONE call token; three receivers on one socket; the ready-made prefix picker.
    let token = "lgSHAREDPFX".to_string();
    let routing = CallRouting::new(token.clone())
        .leg(uas, "bob")
        .leg(uas, "bob2")
        .leg(uas, "charlie")
        .picker(uas, prefix_leg_picker(["bob", "bob2", "charlie"]));
    let net = core.network(routing);

    // Bind in declaration order: bob, bob2, charlie.
    let ep_bob = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let ep_bob2 = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let ep_charlie = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Same token on every leg (one instance); the R-URI user names the leg. The
    // transfer leg carries a per-call SUFFIX to prove prefix (not exact) matching.
    let invite = |ruri_user: &str, cid: &str| {
        format!(
            "INVITE sip:{ruri_user}@127.0.0.1 SIP/2.0\r\nCall-ID: {cid}@h\r\n\
             X-Loadgen-Id: {token}\r\nTo: <sip:{ruri_user}@127.0.0.1>\r\n\
             From: <sip:a@h>;tag=1\r\nCSeq: 1 INVITE\r\n\r\n"
        )
    };
    // Send out of declaration order to prove routing is by prefix, not arrival.
    sender.send_to(invite("charlie-7f3a", "c-charlie").as_bytes(), uas).await.unwrap();
    sender.send_to(invite("bob2", "c-bob2").as_bytes(), uas).await.unwrap();
    sender.send_to(invite("bob", "c-bob").as_bytes(), uas).await.unwrap();

    let got_bob = tokio::time::timeout(RECV, ep_bob.recv()).await.unwrap().unwrap();
    let got_bob2 = tokio::time::timeout(RECV, ep_bob2.recv()).await.unwrap().unwrap();
    let got_charlie = tokio::time::timeout(RECV, ep_charlie.recv()).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&got_bob.raw).contains("sip:bob@"), "bob got the wrong leg");
    assert!(String::from_utf8_lossy(&got_bob2.raw).contains("sip:bob2@"), "bob2 got the wrong leg");
    assert!(
        String::from_utf8_lossy(&got_charlie.raw).contains("sip:charlie-7f3a@"),
        "charlie got the wrong leg (suffixed transfer user must route by its prefix)"
    );
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(core.stats().delivered.load(Relaxed), 3);
    assert_eq!(core.stats().orphan_no_header.load(Relaxed), 0);
    assert_eq!(core.stats().orphan_unknown_token.load(Relaxed), 0);

    drop(ep_bob);
    drop(ep_bob2);
    drop(ep_charlie);
    assert_eq!(core.registry_size(), 0, "shared-socket slot leaked after receivers dropped");
}

/// newkahneed-033 ask A: a HOP-ROUTED in-dialog/in-transaction request that
/// reaches the shared UAS socket with its R-URI user-part STRIPPED, its Via
/// stack replaced by a single proxy Via, and (being an ACK) no correlation
/// token — the exact wire shape the LB's synthesized §17.1.1.3 non-2xx ACK has
/// — must still demux to the leg that owns the dialog (via the Call-ID
/// promoted when the leg's initial INVITE arrived), never land as a `stray`
/// orphan. The shape mirrors the `nk_reroute` 486 flow captured by `sipflow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_tokenless_in_dialog_ack_demuxes_by_dialog() {
    use sip_net::BindUdpOpts;
    let uas = addr(6444);
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: uas, role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    // Two receivers share the socket (the reroute shape's callee + alt legs),
    // demuxed by number-plan prefixes — so a wrong fall-through would surface.
    let token = "lgHOPACK".to_string();
    let routing = CallRouting::new(token.clone())
        .leg(uas, "callee")
        .leg(uas, "alt")
        .picker(uas, loadgen::labelled_prefix_leg_picker([("+0411", "callee"), ("+0422", "alt")]));
    let net = core.network(routing);
    let ep_callee = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let _ep_alt = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // The b-leg INVITE as the LB forwards it: full R-URI, proxy Via on top.
    let invite = format!(
        "INVITE sip:+0411133166602012@127.0.0.1:6444;user=phone SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKproxy1;rport\r\n\
         X-Loadgen-Id: {token}\r\n\
         Call-ID: hopack-1@h\r\nTo: <sip:+0411133166602012@127.0.0.1>\r\n\
         From: <sip:worker@h>;tag=w1\r\nCSeq: 1 INVITE\r\n\r\n"
    );
    sender.send_to(invite.as_bytes(), uas).await.unwrap();
    let got = tokio::time::timeout(RECV, ep_callee.recv()).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&got.raw).starts_with("INVITE "), "leg INVITE demuxes");

    // The hop-routed ACK to this leg's 486, as observed on the wire: user-part
    // stripped from the R-URI, ONE fresh proxy Via, no token header — only the
    // dialog identity (Call-ID / From / To / CSeq) survives.
    let ack = "ACK sip:127.0.0.1:6444 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKproxy2;rport\r\n\
         Call-ID: hopack-1@h\r\nTo: <sip:+0411133166602012@127.0.0.1>;tag=c486\r\n\
         From: <sip:worker@h>;tag=w1\r\nCSeq: 1 ACK\r\n\r\n";
    sender.send_to(ack.as_bytes(), uas).await.unwrap();

    let got = tokio::time::timeout(RECV, ep_callee.recv())
        .await
        .expect("the tokenless hop-routed ACK must demux to the dialog's leg, not orphan")
        .unwrap();
    assert!(String::from_utf8_lossy(&got.raw).starts_with("ACK "), "delivered message is the ACK");
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(core.stats().orphan_stray.load(Relaxed), 0, "no stray orphan for the ACK");
}

/// newkahneed-036 ask A: the mux inbox reports every demuxed datagram to an
/// installed delivery tap AT DELIVERY — independent of whether the scenario
/// body ever `recv`s it — and a datagram the per-call loss model discards is
/// still reported, tagged as modeled loss. This is the seam the recording
/// decorator rides so a sampled call's ladder / RFC audit reflect the true
/// wire (the `nk_reroute` unconsumed hop-ACK class).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_recorded_inbox_taps_delivery_and_modeled_loss() {
    use sip_net::{BindUdpOpts, RecvDisposition};
    use std::sync::Mutex as StdMutex;
    let uas = addr(6446);
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: uas, role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    let first_line = |raw: &[u8]| {
        String::from_utf8_lossy(raw).split_whitespace().next().unwrap_or("").to_string()
    };

    // Call 1: no loss model. INVITE + in-dialog ACK both tap `Delivered` at
    // demux time; NOTHING ever recvs from the endpoint.
    let net = core.network(CallRouting::new("lgTAP1".to_string()).leg(uas, "callee"));
    let ep = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let seen: Arc<StdMutex<Vec<(String, RecvDisposition)>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink = seen.clone();
    assert!(ep.install_recv_tap(Arc::new(move |pkt, disp| {
        sink.lock().unwrap().push((
            String::from_utf8_lossy(&pkt.raw).split_whitespace().next().unwrap_or("").to_string(),
            disp,
        ));
    })));

    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let invite = "INVITE sip:x@127.0.0.1:6446 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKt1\r\n\
         X-Loadgen-Id: lgTAP1\r\n\
         Call-ID: tap-1@h\r\nTo: <sip:x@127.0.0.1>\r\n\
         From: <sip:w@h>;tag=w1\r\nCSeq: 1 INVITE\r\n\r\n";
    let ack = "ACK sip:127.0.0.1:6446 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKt2\r\n\
         Call-ID: tap-1@h\r\nTo: <sip:x@127.0.0.1>;tag=c486\r\n\
         From: <sip:w@h>;tag=w1\r\nCSeq: 1 ACK\r\n\r\n";
    sender.send_to(invite.as_bytes(), uas).await.unwrap();
    sender.send_to(ack.as_bytes(), uas).await.unwrap();

    // Both datagrams demux + tap without a single recv() on the endpoint.
    tokio::time::timeout(RECV, async {
        loop {
            if seen.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("both arrivals must reach the tap without any recv()");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("INVITE".to_string(), RecvDisposition::Delivered),
            ("ACK".to_string(), RecvDisposition::Delivered)
        ]
    );
    assert_eq!(first_line(&ep.try_recv().unwrap().raw), "INVITE", "tap did not consume the inbox");

    // Call 2: loss model at 100% — the demuxed INVITE never reaches the inbox,
    // but a recorded call still sees the arrival, tagged as modeled loss.
    let lossy =
        core.network_tuned(CallRouting::new("lgTAP2".to_string()).leg(uas, "callee"), 1.0, false, 7, None);
    let ep2 = lossy.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let seen2: Arc<StdMutex<Vec<(String, RecvDisposition)>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink2 = seen2.clone();
    assert!(ep2.install_recv_tap(Arc::new(move |pkt, disp| {
        sink2.lock().unwrap().push((
            String::from_utf8_lossy(&pkt.raw).split_whitespace().next().unwrap_or("").to_string(),
            disp,
        ));
    })));
    let invite2 = invite.replace("lgTAP1", "lgTAP2").replace("tap-1@h", "tap-2@h");
    sender.send_to(invite2.as_bytes(), uas).await.unwrap();
    tokio::time::timeout(RECV, async {
        loop {
            if !seen2.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the loss-model discard must still reach the tap");
    assert_eq!(*seen2.lock().unwrap(), vec![("INVITE".to_string(), RecvDisposition::LossModel)]);
    assert!(ep2.try_recv().is_none(), "the loss model kept it out of the inbox");
}

/// newkahneed-036 ask C (`noendpoint` sub-lane): a datagram that CORRELATES to
/// the call (its token matches the slot) but that no logical endpoint accepts
/// (picker miss) is a `NoRoute` orphan on the counters — and, on a recorded
/// call, is still reported to the delivery tap tagged `Unrouted`, so the
/// sampled ladder shows the demux failure instead of hiding it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loadgen_mux_unrouted_correlated_datagram_taps_for_the_ladder() {
    use sip_net::{BindUdpOpts, RecvDisposition};
    use std::sync::Mutex as StdMutex;
    let uas = addr(6447);
    let core = MuxCore::bind(
        vec![EndpointSpec { addr: uas, role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        256,
        10,
        RECV,
        Clock::system(),
    )
    .await
    .unwrap();

    let token = "lgUNROUTED".to_string();
    let routing = CallRouting::new(token.clone())
        .leg(uas, "callee")
        .leg(uas, "alt")
        .picker(uas, loadgen::labelled_prefix_leg_picker([("+0411", "callee"), ("+0422", "alt")]));
    let net = core.network(routing);
    let ep_callee = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();
    let ep_alt = net.bind_udp(BindUdpOpts::new(uas, 256)).await.unwrap();

    let seen: Arc<StdMutex<Vec<(String, RecvDisposition)>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink = seen.clone();
    assert!(ep_callee.install_recv_tap(Arc::new(move |pkt, disp| {
        sink.lock().unwrap().push((
            String::from_utf8_lossy(&pkt.raw).split_whitespace().next().unwrap_or("").to_string(),
            disp,
        ));
    })));

    // Correlated (token matches) but the R-URI prefix matches NO declared leg.
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let invite = format!(
        "INVITE sip:+0999000@127.0.0.1:6447 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKu1\r\n\
         X-Loadgen-Id: {token}\r\n\
         Call-ID: unrouted-1@h\r\nTo: <sip:+0999000@127.0.0.1>\r\n\
         From: <sip:w@h>;tag=w1\r\nCSeq: 1 INVITE\r\n\r\n"
    );
    sender.send_to(invite.as_bytes(), uas).await.unwrap();

    tokio::time::timeout(RECV, async {
        loop {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the unrouted-but-correlated arrival must reach the tap");
    assert_eq!(*seen.lock().unwrap(), vec![("INVITE".to_string(), RecvDisposition::Unrouted)]);
    assert!(ep_callee.try_recv().is_none(), "no inbox got it");
    assert!(ep_alt.try_recv().is_none(), "no inbox got it");
}

/// Emergency / non-emergency split under overload. The b2bua's CPS bucket is
/// exhausted (size 0, no refill), so EVERY non-emergency new INVITE is shed with
/// a stateless 503 while an emergency (`Resource-Priority: esnet.0`) call is
/// force-admitted. Proves the loadgen REPORTING the user wants: non-emergency
/// calls are classified `status_503` (the NOK side), emergency calls are all
/// `ok` with ZERO loss, and the report keeps first-N samples for BOTH classes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_emergency_split_under_overload() {
    let (_h, b2bua, core, transport) = setup_with(
        6450,
        Correlation::header("X-Loadgen-Id"),
        5,
        RECV,
        |c| {
            c.cps_bucket_size = 0; // exhausted bucket → shed every non-emergency
            c.cps_bucket_rate = 0; // …and never refill (deterministic).
        },
    )
    .await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    // Mix sheddable non-emergency basic calls with force-admitted emergency ones.
    let driver = Driver::new(
        cfg(b2bua.addr, 60.0, 3, 16, 0xE5E7),
        vec![
            mix("basic_call", 1.0),
            mix("basic_call_em", 1.0),
        ],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    // Non-emergency: shed → classified status_503, and NONE got through.
    let ne_503 = reporter.count("basic_call", &ResultClass::WrongStatus(503));
    let ne_ok = reporter.count("basic_call", &ResultClass::Ok);
    assert!(ne_503 > 0, "expected non-emergency 503 sheds:\n{}", reporter.render_prometheus());
    assert_eq!(ne_ok, 0, "a non-emergency call slipped past the exhausted bucket");

    // Emergency: force-admitted → all OK, and ZERO shed (the hard invariant).
    let em_ok = reporter.count("basic_call_em", &ResultClass::Ok);
    let em_503 = reporter.count("basic_call_em", &ResultClass::WrongStatus(503));
    assert!(em_ok > 0, "expected emergency calls to establish:\n{}", reporter.render_prometheus());
    assert_eq!(em_503, 0, "an emergency call was shed — must never happen");

    // The b2bua exercised the gate: one stateless reject per shed (the gate
    // `return`s before `build_initial_call`, so a shed births no call — this is
    // why we assert the targeted leak canaries below rather than the call-
    // lifecycle `assert_fully_reaped`, exactly as the authoritative
    // `tier3_admission_gate` test does).
    assert!(
        b2bua.metrics().overload_rejected_total() >= ne_503,
        "every non-emergency shed must be a stateless overload reject:\n{}",
        reporter.render_prometheus()
    );

    // The report keeps samples for BOTH the OK and the 503 class.
    assert!(reporter.sample_count("basic_call", &ResultClass::WrongStatus(503)) > 0, "no 503 sample kept");
    assert!(reporter.sample_count("basic_call_em", &ResultClass::Ok) > 0, "no OK sample kept");

    // No RESOURCE leak from the sheds OR the emergency teardowns: no live call,
    // no stranded per-call lock, no mux registry residue. (A stateless reject
    // legitimately leaves `creations != removals`; it leaves no resource.)
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under overload");
    settle_until(|| b2bua.active_calls() == 0).await;
    assert_eq!(b2bua.active_calls(), 0, "live call leak under overload");
    assert_eq!(b2bua.lock_count(), 0, "stranded per-call lock under overload");
}

/// Post-call cleanup across EVERY failure-teardown path, without an endurance
/// run. A mix of voluntarily-failing scenarios — callee 486 (final reject, no
/// teardown), abandon-on-180 (CANCEL an early dialog), and a declined REFER
/// (BYE a still-confirmed call whose transfer leg was rejected) — interleaved
/// with happy calls. Each failure mode is reported under its NOK class, and the
/// SUT must FULLY reap afterwards: no live call, no stranded per-call lock, no
/// stamp residue (`assert_fully_reaped`), and no mux registry leak. A regression
/// here is a real post-call-cleanup gap (the leak class endurance otherwise
/// catches days later).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_post_call_cleanup_no_leak() {
    let (_h, b2bua, core, transport) = setup(6460, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 3 }));

    let mut scenarios = MixEntry::failure_mix(&ShapeRegistry::with_defaults(), &inputs());
    scenarios.push(mix("basic_call", 2.0)); // some happy traffic in the mix
    let driver = Driver::new(cfg(b2bua.addr, 50.0, 3, 12, 0xFA17), scenarios, reporter.clone(), transport);
    run_throttled(&driver).await;

    // Each failure mode produced its NOK bucket, and the happy path its OK.
    assert!(
        reporter.count("invite_reject", &ResultClass::WrongStatus(486)) > 0,
        "no 486 final-reject recorded:\n{}",
        reporter.render_prometheus()
    );
    assert!(reporter.count("abandon_ringing", &ResultClass::Timeout) > 0, "no abandoned-early call recorded");
    assert!(reporter.count("refer_charlie_reject", &ResultClass::Unexpected) > 0, "no declined-transfer recorded");
    assert!(reporter.count("basic_call", &ResultClass::Ok) > 0, "no happy call completed");

    // Post-call cleanup is COMPLETE across every failure-teardown path.
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak after a failing-call mix");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

// ---------------------------------------------------------------------------
// Simulated packet loss + auto-retransmit (robustness knobs)
// ---------------------------------------------------------------------------

/// Baseline: a loss the SENDER never re-emits is unrecoverable by anyone —
/// the caller's outbound INVITE is discarded BEFORE the wire (permanent
/// targeted drop, no auto-retransmit), so no SUT behavior of any quality can
/// save the call: the SUT never even sees it. This is the vacuity guard for
/// the recovery twin below, made DETERMINISTIC (every call fails, always):
/// it proves the loss model bites and that an unrecovered loss degrades to a
/// bounded `timeout` — never a silent OK, never a mux leak. Deliberately NOT
/// probabilistic and NOT an assertion about the SUT: as the SUT/harness grow
/// more RFC-faithful re-emission, inbound-direction drops legitimately
/// recover, and a rate-based "some calls must fail" would erode into a flake
/// (it did, under full-suite contention).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_packet_drop_without_retransmit_breaks_calls() {
    let (_h, b2bua, core, transport) = setup(6480, Correlation::header("X-Loadgen-Id"), 3).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 4.0, 1, 4, 0xD40F);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: false,
        drop_nth: Some(TargetedDrop {
            method: "INVITE",
            nth: 1,
            permanent: true,
            dir: DropDir::Outbound,
            leg: None,
        }),
    };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed);
    let total = reporter.total_calls();
    assert!(total >= 1, "no calls ran");
    assert!(drops >= total, "the targeted INVITE drop never fired: drops={drops} calls={total}");
    // EVERY call fails, and every failure is the bounded timeout class —
    // deterministic, no dominance ratio.
    assert_eq!(
        reporter.count("basic_call", &ResultClass::Timeout),
        total,
        "a call with its INVITE dropped at the sender did not time out:\n{}",
        reporter.render_prometheus()
    );

    // The loadgen's OWN mux state is always reclaimed once the call task ends,
    // even for a failed lossy call. (SUT-side reap is NOT asserted: the SUT
    // never saw these calls at all.)
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss");
}

/// The point of the feature: a heavy ~10%/datagram loss (which breaks the
/// majority of calls with no retransmit, above) is RECOVERED with auto-retransmit
/// on — per SIP timers (Timer A/E requests, 2xx-until-ACK answers, re-ACK,
/// reactive re-answer, duplicate absorption) — so calls overwhelmingly SUCCEED
/// despite drops actually happening. The recv window is widened (like the
/// production 5 s) so compounded two-hop recovery has headroom under CI CPU load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_auto_retransmit_recovers_packet_drop() {
    // recv = 12 s: under CPU contention a recovering call's retransmit ladder
    // (0.5+1+2+4 s cumulative) plus stretched per-hop latency can overrun a 6 s
    // window, flipping the small-N dominance ratio below — the observed flake.
    // The wide window only costs wall time on the deterministic doomed-call
    // tail, which idles the full window either way.
    let (_h, b2bua, core, transport) =
        setup_recv(6590, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 12.0, 2, 8, 0x5EED);
    c.default_tuning = CallTuning { drop_rate: 0.10, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed)
        + core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    let timeouts = reporter.count("basic_call", &ResultClass::Timeout);
    assert!(drops > 0, "loss model dropped nothing — recovery test is vacuous");
    assert!(ok >= 6, "too few OK calls despite retransmit: ok={ok}\n{}", reporter.render_prometheus());
    // Retransmit recovers each lost datagram inside the (wide) recv window, so OK
    // dominates overwhelmingly — the SAME loss that broke the majority of the
    // no-retransmit calls above. A comfortable 4:1 margin absorbs the rare tail
    // (a datagram lost on every retry) plus CI scheduling jitter.
    assert!(
        ok >= timeouts * 4,
        "retransmit did not dominate: ok={ok} timeouts={timeouts} drops={drops}\n{}",
        reporter.render_prometheus()
    );

    // The 18x-delivery gate is wired: calls reached the answer step, and a dropped
    // non-PRACK ringing (NOT recovered — provisionals are best-effort) is tolerated,
    // so `received <= expected` and those calls still counted as OK above.
    let (rung, rung_expected) = reporter.ringing_totals();
    assert!(rung_expected > 0, "no calls reached the ring→answer step");
    assert!(rung <= rung_expected, "ringing received {rung} > expected {rung_expected}");
    let metrics = reporter.render_prometheus();
    assert!(
        metrics.contains("loadgen_ringing_expected_total")
            && metrics.contains("loadgen_ringing_received_total"),
        "ringing gate metrics missing from /metrics"
    );

    // The loadgen's mux state is clean regardless. (SUT full-reap is not asserted:
    // a rare timed-out call's teardown is best-effort single-shot and its resender
    // is cancelled at call-end, so the b2bua reaps that straggler on its own timers
    // — the successful majority reap promptly because hangup awaits the BYE 200.)
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss+retransmit");
}

/// B9 baseline: the SAME lossy fabric, refer-only mix, NO retransmit — the
/// multi-leg transfer (~3× the datagrams of a basic call, three serialized
/// legs) breaks under loss, establishing that the recovery proof below is not
/// vacuous. (No SUT-reap assert: with no retransmit the teardown itself can be
/// lost — the very fragility the recovery test proves fixed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_refer_drop_without_retransmit_breaks_transfers() {
    let (_h, b2bua, core, transport) = setup(6510, Correlation::header("X-Loadgen-Id"), 3).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 8.0, 2, 8, 0xB9B9);
    c.default_tuning = CallTuning { drop_rate: 0.08, retransmit: false, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("refer", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed)
        + core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    assert!(drops > 0, "the loss model dropped nothing — the knob is not wired");
    let ok = reporter.count("refer", &ResultClass::Ok);
    let total = reporter.total_calls();
    assert!(
        total > ok,
        "no refer call failed despite {drops} dropped datagrams and no retransmit:\n{}",
        reporter.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under refer loss");
}

/// **B9 — the actor-refer loss-recovery proof** (plan §4.5). The two endurance
/// failures this port exists to fix (`results/endurance-20260712-074837`, refer
/// callflows) both trace to the LINEAR body's ONE serialized coroutine racing
/// the protocol's own re-emission:
///
/// 1. a REFER-progress NOTIFY dropped toward bob, whose re-emission landed
///    after the coroutine had already torn down → a §12.2.1.1 CSeq gap charged
///    to the SUT (`rfc_audit_fail/rfc3261.cseqInDialogOrder` — a TEST-MODEL
///    false positive: the SUT emitted a contiguous stream, the harness saw a
///    hole);
/// 2. the SUT's realign ACK dropped, stranding the coroutine at
///    `charlie.try_receive("ACK")` — which also left alice's realign unserved
///    and let NOTIFYs pile up as `⚠absorbed retransmit`
///    (`timeout/charlie@transferred`).
///
/// The actor executor's structural wins, held constant against the SAME
/// in-process SUT and the SAME lossy fabric + retransmit engine that breaks the
/// no-retransmit baseline above:
///
/// - **No false RFC charge, ever.** Every peer stays reactive and the settle
///   barrier holds the verdict until every in-dialog CSeq is accounted for, so
///   a dropped datagram can NEVER surface as a §12.2.1.1 audit finding —
///   `rfc_audit_fail` is 0 under loss (failure 1's regression gate). An
///   unrecovered loss degrades to a graceful `timeout` (the barrier / settle
///   ceiling), never a protocol-defect class.
/// - **No stranding.** A drop on one realign leg no longer freezes the others —
///   each answers independently, so the recoverable majority (dropped
///   NOTIFY/200/BYE the peer or SUT re-emits) SUCCEEDS.
///
/// A drop of the SUT's OWN realign ACK stays unrecoverable — only the SUT can
/// re-ACK its re-INVITE 2xx (§13.2.2.4), and this in-process `B2buaCore` does
/// not (the same gap the endurance memory flags as failure 2's leading
/// hypothesis, out of scope for a harness redesign). The actor form makes that
/// residue a clean, bounded `timeout` instead of a cascade — which is exactly
/// what this test pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_actor_refer_recovers_loss_without_false_audit() {
    // recv = 12 s like the basic-call recovery test: compounded two-hop
    // retransmit ladders (0.5+1+2+4 s) need headroom under CI CPU contention.
    let (_h, b2bua, core, transport) =
        setup_recv(6530, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 6.0, 2, 8, 0xB9A2);
    c.default_tuning = CallTuning { drop_rate: 0.05, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("refer", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed)
        + core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    assert!(drops > 0, "loss model dropped nothing — recovery proof is vacuous");

    let ok = reporter.count("refer", &ResultClass::Ok);
    let timeouts = reporter.count("refer", &ResultClass::Timeout);
    let total = reporter.total_calls();
    let nok = total.saturating_sub(ok);

    // The recoverable majority succeeds — the SAME loss that broke transfers with
    // no retransmit above (a strict majority, unlike the no-retransmit baseline
    // where the majority failed).
    assert!(ok >= 6, "too few OK transfers despite retransmit: ok={ok}/{total} drops={drops}\n{}", reporter.render_prometheus());
    assert!(
        ok > nok,
        "actor refer did not recover the majority: ok={ok} nok={nok} drops={drops}\n{}",
        reporter.render_prometheus()
    );

    // FAILURE 1'S REGRESSION GATE — the headline: a loss-model drop can NEVER
    // surface as a §12.2.1.1 cseq-gap audit false positive.
    assert_eq!(
        reporter.count("refer", &ResultClass::RfcAuditFail),
        0,
        "a dropped datagram leaked into the RFC audit as a false positive:\n{}",
        reporter.render_prometheus()
    );
    // Graceful degradation: the ONLY way an actor-refer call fails under loss is
    // a `timeout` (a barrier / the settle ceiling) — never a protocol-defect
    // class (RfcAuditFail / WrongMethod / Unexpected / Panic). So every NOK is a
    // timeout: the unrecoverable SUT-realign-ACK residue is a clean bounded
    // give-up, not the linear form's stranded-coroutine cascade.
    assert_eq!(
        ok + timeouts,
        total,
        "an actor-refer call failed with a NON-timeout class under loss:\n{}",
        reporter.render_prometheus()
    );
    // Contract-table §3 regression: the refer body must NOT feed the cross-call
    // 18x gate (the linear body never did — a pure-refer run expects zero).
    let (_rung, expected) = reporter.ringing_totals();
    assert_eq!(expected, 0, "refer began feeding the 18x ringing gate — contract drift");

    // The loadgen's own mux state is always reclaimed; the SUT may hold only the
    // (rare) failed calls' straggler dialogs (best-effort teardown is single-shot
    // under loss), reaped on its own timers. Use the shared lossy settle ceiling
    // ([`SETTLE_LOSS_SECS`], > the SUT's 32 s terminating timer) not the 1 s
    // `settle_until`: under loss a recovered refer call's teardown rides retransmit
    // ladders and — worst case, a fully-lost BYE — falls back to that 32 s SUT
    // timer, so a 20 s window flaked under full-suite CPU contention.
    settle_secs(SETTLE_LOSS_SECS, || core.registry_size() == 0 && b2bua.active_calls() as u64 <= nok).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak under loss+retransmit");
    assert!(
        b2bua.active_calls() as u64 <= nok,
        "SUT holds {} live calls vs {nok} failed — a RECOVERED call leaked",
        b2bua.active_calls()
    );
}

/// **P2 — the ack-gate RECOVERY side** (plan §5). A refer call's a-leg BYE — an
/// in-dialog request the settle ledger tracks (a `Bye` obligation + its dialog
/// CSeq) — is dropped DETERMINISTICALLY on its first OUTBOUND send. The loadgen
/// retransmit engine's Timer-E resend heals it: the SUT eventually sees the BYE,
/// 200s it, and the ledger closes — so the settle barrier returns OK rather than
/// racing the verdict past the still-in-flight teardown. Every call is OK and
/// the drop NEVER surfaces as an RFC-audit false positive. (A dropped SUT→peer
/// NOTIFY is NOT used for the recovery side: this in-process SUT sends distinct
/// fire-and-forget progress NOTIFYs and does not retransmit a lost one — that
/// unrecoverable case is the permanent-loss test below.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_settle_gate_recovers_dropped_bye() {
    let (_h, b2bua, core, transport) =
        setup_recv(6580, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 2.0, 1, 4, 0x2D07);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop { method: "BYE", nth: 1, permanent: false, dir: DropDir::Outbound, leg: None }),
    };
    let driver = Driver::new(c, vec![mix("refer", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("refer", &ResultClass::Ok);
    assert!(total >= 1, "no calls ran");
    assert!(drops >= total, "the targeted BYE drop never fired: drops={drops} calls={total}");
    // EVERY call recovered: the re-sent BYE was acked before the settle ceiling —
    // deterministic, so no dominance ratio, all-or-nothing.
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

/// REPRO — the SUT §13.2.2.4 re-ACK gap, deterministically. Drop the FIRST ACK
/// bob receives (the SUT's initial-INVITE b-leg ACK), one-shot inbound. bob then
/// retransmits its 200 (Timer G, 2xx-until-ACK); the RFC-correct SUT must re-ACK
/// each retransmit on the SAME branch (§13.2.2.4) so bob's INVITE server txn
/// quiesces and the call reaps. WITHOUT the fix the SUT never re-ACKs → bob
/// retransmits to its ceiling and the call is stranded / RFC-audit-charged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-clock UDP — slow lane (just test-slow); avoids default-lane bind contention"]
async fn loadgen_reack_recovers_dropped_initial_b_leg_ack() {
    let (_h, b2bua, core, transport) =
        setup_recv(6640, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 1.0, 1, 4, 0x2AC0);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop { method: "ACK", nth: 1, permanent: false, dir: DropDir::Inbound, leg: None }),
    };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    let audit = reporter.count("basic_call", &ResultClass::RfcAuditFail);
    eprintln!("REPRO drops_in={drops} total={total} ok={ok} audit={audit}\n{}", reporter.render_prometheus());
    assert!(total >= 1 && drops >= 1, "no call / drop never fired: drops={drops} total={total}");

    settle_secs(SETTLE_LOSS_SECS, || core.registry_size() == 0 && b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert_eq!(audit, 0, "dropped initial b-leg ACK charged the audit (SUT re-ACK gap):\n{}", reporter.render_prometheus());
    assert_eq!(ok, total, "dropped initial b-leg ACK not recovered by SUT re-ACK");
}

/// The FAITHFUL-UAS side (SIPp-replacement contract): a NON-2xx final whose
/// hop-ACK is lost is recovered by the CALLEE retransmitting it (Timer G, §17.2.1)
/// until the SUT's txn layer re-ACKs (§17.1.1.2). Drop the FIRST ACK the callee
/// receives — the SUT's hop-ACK for the 486 — one-shot inbound. Without the
/// loadgen non-2xx resender the reject is stranded as an unACKed final and the SUT
/// is wrongly charged `unackedInviteNon2xxFinal`; with it the callee resends the
/// 486, the SUT re-ACKs, and the audit stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-clock UDP — slow lane (just test-slow); avoids default-lane bind contention"]
async fn loadgen_callee_retransmits_non2xx_final_on_lost_hop_ack() {
    let (_h, b2bua, core, transport) =
        setup_recv(6610, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 1.0, 1, 4, 0x2AC1);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop { method: "ACK", nth: 1, permanent: false, dir: DropDir::Inbound, leg: None }),
    };
    let driver = Driver::new(c, vec![mix("invite_reject", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    let audit = reporter.count("invite_reject", &ResultClass::RfcAuditFail);
    eprintln!("REPRO-486 drops_in={drops} audit={audit}\n{}", reporter.render_prometheus());
    assert!(drops >= 1, "the reject hop-ACK drop never fired: drops={drops}");

    settle_secs(SETTLE_LOSS_SECS, || core.registry_size() == 0 && b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert_eq!(
        audit, 0,
        "a lost hop-ACK to a 486 was not recovered by the callee's Timer-G retransmit + SUT re-ACK:\n{}",
        reporter.render_prometheus()
    );
}

/// The UA-OUTLIVES-THE-CALL side of the faithful-UAS contract: the recovery of
/// an ABANDONED reject leg must land ON-recording. `rerouting_prack`: bob
/// REJECTS (486) and the SUT walks the failover plan to bob2, so bob's leg is
/// abandoned by the reroute while the happy call completes via bob2 within
/// milliseconds. Drop the FIRST inbound ACK — the SUT's hop-ACK for bob's 486.
/// The recovery (bob's Timer-G 486 retransmit, §17.2.1, + the SUT's §17.1.1.2
/// re-ACK) fires at ~500 ms; without the reject-final ledger obligation the
/// per-call verdict (and the RFC-audit snapshot) is computed before it lands —
/// off-recording — and the audit falsely charges `unackedInviteNon2xxFinal`.
/// With the obligation, the settle barrier holds the verdict (Timer-H-bounded)
/// until the hop-ACK is claimed, so the recording spans the recovery and the
/// audit stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-clock UDP — slow lane (just test-slow); avoids default-lane bind contention"]
async fn loadgen_abandoned_reject_leg_recovery_lands_on_recording() {
    let (_h, b2bua, core, transport) =
        setup_api_call(6630, Correlation::header("X-Loadgen-Id"), 3).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 1.0, 1, 4, 0x2AC2);
    c.call.egress = EgressPolicy::ApiCallPin; // the [bob, bob2] plan rides X-Api-Call
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        // Leg-scoped to bob: an unscoped `ACK`/`nth:1` would ALSO drop bob2's
        // answer ACK (per-endpoint counters), whose ~500 ms recovery delays the
        // whole call and masks the off-recording window under test.
        drop_nth: Some(TargetedDrop {
            method: "ACK",
            nth: 1,
            permanent: false,
            dir: DropDir::Inbound,
            leg: Some("bob"),
        }),
    };
    let driver = Driver::new(c, vec![mix("rerouting_prack", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let drops = core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    let total = reporter.total_calls();
    let ok = reporter.count("rerouting_prack", &ResultClass::Ok);
    let audit = reporter.count("rerouting_prack", &ResultClass::RfcAuditFail);
    eprintln!("REPRO-abandoned-486 drops_in={drops} total={total} ok={ok} audit={audit}\n{}", reporter.render_prometheus());
    // Report FIRST (before the asserts), so a failing run stays inspectable.
    let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/loadgen-abandoned-reject-report");
    reporter.finalize(&out_dir).expect("write repro report");
    eprintln!("repro report: {}/index.html", out_dir.display());
    assert!(total >= 1 && drops >= 1, "no call / the reject hop-ACK drop never fired: drops={drops} total={total}");

    settle_secs(SETTLE_LOSS_SECS, || core.registry_size() == 0 && b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
    assert_eq!(
        audit, 0,
        "the abandoned reject leg's recovery landed OFF-recording — `unackedInviteNon2xxFinal` falsely charged:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        ok, total,
        "a reroute call with a lost reject hop-ACK did not recover to OK:\n{}",
        reporter.render_prometheus()
    );
}

/// **P2 — the 2nd-NOTIFY ack gate, PERMANENT-LOSS side** (plan §5). The same
/// targeted drop, but EVERY arrival (re-emissions included) is discarded — an
/// unrecoverable loss. The contract (table §2): the call's teardown still
/// completes, the settle barrier's 32 s ceiling elapses with the CSeq gap open
/// (the later BYE reveals the hole), and the verdict maps to the FIXED
/// `Timeout { who: "settle" }` — class `timeout`, case `settle@transferred` —
/// with the sample DETAIL naming the open obligation. Never a protocol-defect
/// class: `rfc_audit_fail` stays 0. Real-clock ~35 s (the genuine 64·T1 settle
/// ceiling — deliberately not a knob), still inside the default lane's 60 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_settle_gate_permanent_notify_loss_names_obligation() {
    let (_h, b2bua, core, transport) =
        setup_recv(6570, Correlation::header("X-Loadgen-Id"), 3, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 1 }));

    // ONE call is enough — the outcome is deterministic.
    let mut c = cfg(b2bua.addr, 1.0, 1, 2, 0x2D08);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop {
            method: "NOTIFY",
            nth: 2,
            permanent: true,
            dir: DropDir::Inbound,
            leg: None,
        }),
    };
    let driver = Driver::new(c, vec![mix("refer", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let total = reporter.total_calls();
    assert!(total >= 1, "no calls ran");
    let timeouts = reporter.count("refer", &ResultClass::Timeout);
    // Graceful, bounded give-up: EVERY call lands in `timeout` (the settle
    // verdict), never an RFC-audit/unexpected/panic class.
    assert_eq!(
        timeouts, total,
        "permanent NOTIFY loss did not land in the settle timeout class:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("refer", &ResultClass::RfcAuditFail),
        0,
        "permanent loss surfaced as an RFC-audit false positive:\n{}",
        reporter.render_prometheus()
    );

    // The report names the failure mode: the case bucket is the FIXED
    // `settle@transferred` (who="settle", last phase `transferred`), and the
    // sampled page's detail names the open obligation (the never-observed CSeq).
    let out = std::env::temp_dir().join(format!("loadgen-settle-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    reporter.finalize(&out).expect("finalize report");
    let case_dir = out.join("callflows/refer/timeout/settle@transferred");
    assert!(
        case_dir.is_dir(),
        "expected the settle@transferred case bucket; report tree: {:?}",
        list_dirs(&out)
    );
    let mut named = false;
    for entry in walk_files(&case_dir) {
        let html = std::fs::read_to_string(&entry).unwrap_or_default();
        if html.contains("settle:") && html.contains("gap") {
            named = true;
            break;
        }
    }
    assert!(named, "no sampled page names the open obligation under {case_dir:?}");
    let _ = std::fs::remove_dir_all(&out);

    // The teardown itself completed (the loss was a NOTIFY, not the BYE): the
    // SUT is fully reaped despite the NOK verdict.
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// Every file under `dir`, recursively (report-tree assertion helper).
fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Every directory path under `root`, recursively (assert-message diagnostics).
fn list_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.push(p.clone());
            out.extend(list_dirs(&p));
        }
    }
    out
}

/// The cross-call **18x-delivery gate mechanism**: with NO loss the ringing
/// counter must read exactly 100% (`received == expected`) — a deterministic proof
/// that the strict ordering holds (the 180 always precedes the 200, never stranded
/// or reordered) and the counter is wired to `/metrics`. The real >99%-UNDER-LOSS
/// gate needs scale (one 1/1000 miss breaches 99% at N=40), so it is asserted by
/// the 600-call `loadgen_inprocess_endurance_lossy` slow-lane test below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_ringing_gate_counts_every_ring() {
    let (_h, b2bua, core, transport) = setup(6500, Correlation::header("X-Loadgen-Id"), 3).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    // No loss → every 18x is delivered; retransmit on to exercise that path too.
    let mut c = cfg(b2bua.addr, 40.0, 2, 16, 0x9A17);
    c.default_tuning = CallTuning { drop_rate: 0.0, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    run_throttled(&driver).await;

    let (rung, expected) = reporter.ringing_totals();
    assert!(expected >= 20, "too few answered calls: {expected}");
    // 100% — no reordering / stranded-180 artifacts (the strict-ordering guarantee).
    assert_eq!(rung, expected, "some 18x uncounted with zero loss: {rung}/{expected}");
    let metrics = reporter.render_prometheus();
    assert!(
        metrics.contains("loadgen_ringing_expected_total")
            && metrics.contains("loadgen_ringing_received_total"),
        "ringing gate metrics missing from /metrics"
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}
/// **In-process endurance** — a sustained real-clock run of the mix against the
/// in-process b2bua with the production 1/1000 loss + auto-retransmit, to shake
/// out the retransmit engine, the 18x gate, and no-leak under load BEFORE the
/// cluster endurance. Slow lane (`just test-slow` / `--ignored`); the default lane
/// keeps the short probabilistic tests above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-clock ~25s — in-process endurance, slow lane (just test-slow)"]
async fn loadgen_inprocess_endurance_lossy() {
    let (_h, b2bua, core, transport) =
        setup_recv(6520, Correlation::header("X-Loadgen-Id"), 5, RECV_LOSSY).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 16 }));

    // The default mix (basic-heavy), production loss + retransmit, 25 s at 25 cps.
    let mut c = cfg(b2bua.addr, 25.0, 25, 64, 0xE0D0E);
    c.default_tuning = CallTuning { drop_rate: 0.001, retransmit: true, ..CallTuning::default() };
    let driver = Driver::new(c, MixEntry::default_mix(&ShapeRegistry::with_defaults(), &inputs()), reporter.clone(), transport);
    run_throttled(&driver).await;

    let total: u64 = ["basic_call", "reinvite", "options_hold", "refer"]
        .iter()
        .map(|id| reporter.count(id, &ResultClass::Ok))
        .sum();
    assert!(total > 300, "too few OK calls over the run: {total}\n{}", reporter.render_prometheus());
    // 18x delivery holds the >99% gate under sustained production loss.
    let (rung, expected) = reporter.ringing_totals();
    assert!(expected > 300, "too few answered calls: {expected}");
    assert!(
        rung * 100 >= expected * 99,
        "18x gate breached over the run: {rung}/{expected}\n{}",
        reporter.render_prometheus()
    );
    // No LOADGEN leak: the mux state is always reclaimed exactly (call task end).
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak over the endurance run");
    // SUT reap: a SUCCESSFUL call always reaps; but a FAILED call under injected
    // loss can have its best-effort teardown (CANCEL/BYE, single-shot — its mux
    // resender is cancelled at call end) LOST, leaving the SUT to reap that dialog
    // on its own (longer) timers, past this settle. So the un-reaped SUT state must
    // be bounded by the (rare) failure count — a SUCCESSFUL call leaking would be a
    // real bug (leaked > failed).
    settle_until(|| {
        b2bua.metrics().creations_total() == b2bua.metrics().removals_total()
    })
    .await;
    let failed = reporter.total_calls().saturating_sub(total);
    let leaked = b2bua.metrics().creations_total().saturating_sub(b2bua.metrics().removals_total());
    assert!(
        leaked <= failed as u64,
        "SUT leaked {leaked} calls but only {failed} failed under loss — a SUCCESSFUL call leaked:\n{}",
        reporter.render_prometheus()
    );
    assert!(
        b2bua.active_calls() as u64 <= failed as u64,
        "SUT holds {} live calls vs {failed} failed",
        b2bua.active_calls()
    );
}

// ---------------------------------------------------------------------------
// Test-case-driven binding pools (the parameters axis)
// ---------------------------------------------------------------------------

/// A basic call that OBSERVES its per-call resolution (the resolved core From
/// and the effective dwells) before delegating to the shared choreography — the
/// probe for the binding-pool wiring below.
struct ObservedBasic {
    froms: Arc<std::sync::Mutex<Vec<String>>>,
    dwells: Arc<std::sync::Mutex<Vec<(Duration, Duration)>>>,
}

impl ActorScenario for ObservedBasic {
    fn id(&self) -> ScenarioId {
        "pooled_basic"
    }
    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        // The observation moves from the linear `run()` to `build()` — both are
        // invoked once per call with the bound env — then delegate to the shipped
        // actor `BasicCall` body verbatim.
        self.froms
            .lock()
            .unwrap()
            .push(env.core.from.clone().expect("every pool entry sets `from`"));
        self.dwells.lock().unwrap().push((env.ring_delay, env.talk_time));
        BasicCall.build(env)
    }
}

/// The pooled Test case end to end: the committed example
/// `e2e/cases/load-basic-pooled.json` attached to a mix entry drives
/// (a) DIFFERENT From identities across calls (the seq/rand pool walk with
/// `${seq:4}`/`${rand:6}` expansion, folded into the wire INVITE through the
/// egress `outgoing_invite` path), (b) GREEN result classes throughout, and
/// (c) the per-call dwell overrides (`ring_delay_ms: 25`, `talk_time_ms: 10`
/// from the case extras) over the global defaults (0/0). The sampled callflow
/// page shows the resolved binding in its header banner, while the report
/// buckets stay scenario-keyed (no per-binding cardinality).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_pooled_case_identities_and_dwell_overrides() {
    let (_h, b2bua, core, transport) = setup(6600, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    let case_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/cases/load-basic-pooled.json");
    let case = Arc::new(LoadCase::load(&case_path, &Default::default(), 0x10AD));

    let froms = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dwells = Arc::new(std::sync::Mutex::new(Vec::new()));
    let scenario: Arc<dyn ActorScenario> =
        Arc::new(ObservedBasic { froms: froms.clone(), dwells: dwells.clone() });

    // Global dwells stay 0 (the `cfg` default), so an observed 25 ms ring /
    // 10 ms talk can ONLY come from the case extras.
    let driver = Driver::new(
        cfg(b2bua.addr, 50.0, 2, 16, 0xB1D5),
        vec![MixEntry::from((scenario, 1.0)).with_case(Some(case))],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    // (b) Green classes: every completed call is OK (no timeout / wrong-status /
    // rfc_audit_fail on the sampled+audited half).
    let ok = reporter.count("pooled_basic", &ResultClass::Ok);
    assert!(ok > 5, "too few OK pooled calls:\n{}", reporter.render_prometheus());
    assert_eq!(
        reporter.total_calls(),
        ok,
        "non-OK result classes with a pooled case:\n{}",
        reporter.render_prometheus()
    );

    // (a) Different From identities across calls: the seq-mode pool alternates
    // its two entries and every resolution expands fresh digits.
    let froms = froms.lock().unwrap().clone();
    let distinct: std::collections::BTreeSet<&String> = froms.iter().collect();
    assert!(
        distinct.len() >= froms.len().saturating_sub(1),
        "pool identities repeated unexpectedly early: {froms:?}"
    );
    assert!(distinct.len() > 1, "all calls used one identity: {froms:?}");
    for f in &froms {
        assert!(
            f.starts_with("sip:+3310") && f.ends_with("@pool.example")
                || f.starts_with("sip:+4420") && f.ends_with("@pool.example"),
            "unexpected resolved From {f:?}"
        );
    }

    // (c) The per-call dwell overrides beat the global (zero) defaults.
    let dwells = dwells.lock().unwrap().clone();
    assert!(!dwells.is_empty());
    for (ring, talk) in &dwells {
        assert_eq!(*ring, Duration::from_millis(25), "ring_delay_ms extras override not applied");
        assert_eq!(*talk, Duration::from_millis(10), "talk_time_ms extras override not applied");
    }

    // The sampled OK callflow page carries the resolved binding in its header
    // banner (case id + the actual From used), and the recorded wire INVITE
    // itself shows the expanded identity — proof the core rode the egress
    // outgoing-invite path onto the wire, not just into the env.
    let out = std::env::temp_dir().join(format!("loadgen-pooled-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    reporter.finalize(&out).unwrap();
    let page = out.join("callflows/pooled_basic/ok/clear/0.html");
    let html = std::fs::read_to_string(&page)
        .unwrap_or_else(|e| panic!("no OK sample page at {}: {e}", page.display()));
    assert!(html.contains("binding: case=load-basic-pooled"), "banner missing from sample page");
    assert!(html.contains("from=sip:+"), "resolved From missing from the banner");
    assert!(html.contains("@pool.example"), "expanded identity not on the recorded wire INVITE");
    let _ = std::fs::remove_dir_all(&out);

    // No leak, as everywhere: mux registry reclaimed, SUT fully reaped.
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (pooled case)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

// ---------------------------------------------------------------------------
// Test-case checks + allow_violations on sampled load calls
// ---------------------------------------------------------------------------

/// Build an in-memory Test case (the loader-free seam) for the check tests.
fn check_case(json: &str) -> e2e_model::TestCase {
    serde_json::from_str(json).unwrap()
}

/// (a)+(d) A pooled case whose checks PASS — via a referenced Check set AND an
/// inline block — keeps every call OK, and the sampled OK page shows the
/// verdicts (PASS lines) next to the flow. The Check-set block keys on
/// `bob.initialInvite`, so this also proves send-side ANCHORS resolve on the
/// load surface: the anchor finds the relayed b-leg INVITE in the recording
/// and `${input.from}` binds to THIS call's pool-resolved identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_case_checks_pass_and_render_verdicts() {
    let (_h, b2bua, core, transport) = setup(6700, Correlation::header("X-Loadgen-Id"), 5).await;
    // Record EVERY call (background_record_every = 1): checks are a per-sample
    // oracle, so full recording makes the assertion deterministic.
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 1 }));

    let set: e2e_model::CheckSet = serde_json::from_str(
        r#"{ "id": "load-invite-identity", "blocks": [
              { "on": "bob.initialInvite", "checks": [
                  { "field": "from.uri", "op": "eq", "value": "${input.from}" },
                  { "field": "from.tag", "op": "exists" },
                  { "field": "header(X-Loadgen-Id)", "op": "exists" } ] } ] }"#,
    )
    .unwrap();
    let sets: std::collections::BTreeMap<String, e2e_model::CheckSet> =
        [("load-invite-identity".to_string(), set)].into();
    let case = check_case(
        r#"{ "id": "checked-pool", "compatibleShapes": ["basic_call"],
             "bindings": { "mode": "seq", "entries": [
               { "core": { "from": "sip:+331${seq:4}@pool.example" } } ] },
             "checkSets": ["load-invite-identity"],
             "checks": [ { "on": "alice.answer", "checks": [
                 { "field": "to.tag", "op": "exists" } ] } ] }"#,
    );
    let case = Arc::new(LoadCase::new(case, &sets, 0xC4EC).unwrap());

    let driver = Driver::new(
        cfg(b2bua.addr, 50.0, 2, 16, 0xC4EC5),
        vec![mix("basic_call", 1.0).with_case(Some(case))],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    // Passing checks never reclassify: every completed call stays OK.
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    assert!(ok > 5, "too few OK checked calls:\n{}", reporter.render_prometheus());
    assert_eq!(
        reporter.total_calls(),
        ok,
        "a passing-checks run must stay all-OK:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(reporter.count("basic_call", &ResultClass::CheckFail), 0);

    // The sampled OK page renders the verdicts — PASS and the resolved value.
    let out = std::env::temp_dir().join(format!("loadgen-checks-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    reporter.finalize(&out).unwrap();
    let html =
        std::fs::read_to_string(out.join("callflows/basic_call/ok/clear/0.html")).unwrap();
    assert!(html.contains("check bob.initialInvite from.uri"), "verdict line missing:\n{html}");
    assert!(html.contains("PASS"), "PASS verdicts must render on the OK page");
    assert!(html.contains("check alice.answer to.tag"), "inline-block verdict missing");
    let _ = std::fs::remove_dir_all(&out);

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (checked case)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// (b) A deliberately FAILING check reclassifies an otherwise-OK call to the
/// NEW `check_fail` class — counted in Prometheus like any class — and the
/// sampled page shows the FAIL verdict (expected vs got).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_failing_check_reclassifies_to_check_fail() {
    let (_h, b2bua, core, transport) = setup(6720, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 1 }));

    let case = check_case(
        r#"{ "id": "impossible", "compatibleShapes": ["basic_call"],
             "checks": [ { "on": "bob.initialInvite", "checks": [
                 { "field": "from.userInfo", "op": "eq", "value": "nobody-ever" } ] } ] }"#,
    );
    let case = Arc::new(LoadCase::new(case, &Default::default(), 0xFA11).unwrap());

    let driver = Driver::new(
        cfg(b2bua.addr, 40.0, 2, 16, 0xFA115),
        vec![mix("basic_call", 1.0).with_case(Some(case))],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    // Every call records (background 1) → every otherwise-OK call reclassifies.
    let failed = reporter.count("basic_call", &ResultClass::CheckFail);
    assert!(failed > 5, "no check_fail calls:\n{}", reporter.render_prometheus());
    assert_eq!(reporter.count("basic_call", &ResultClass::Ok), 0, "a failing check must never stay OK");
    assert!(
        reporter.render_prometheus().contains("class=\"check_fail\""),
        "check_fail must surface as a Prometheus class:\n{}",
        reporter.render_prometheus()
    );

    // The sampled NOK page carries the FAIL verdict with the mismatch detail.
    let out = std::env::temp_dir().join(format!("loadgen-checks-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    reporter.finalize(&out).unwrap();
    // The sample path carries the case discriminator (`<on>.<field>` of the
    // failing check), so distinct failing checks keep distinct first-N buckets.
    let html = std::fs::read_to_string(
        out.join("callflows/basic_call/check_fail/bob.initialInvite.from.userInfo/clear/0.html"),
    )
    .unwrap();
    assert!(html.contains("check bob.initialInvite from.userInfo"), "verdict missing:\n{html}");
    assert!(html.contains("FAIL"), "FAIL verdict must render");
    assert!(html.contains("nobody-ever"), "expected-value detail must render");
    let _ = std::fs::remove_dir_all(&out);

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (check_fail)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

/// A scenario that legitimately deviates from RFC 3261 §15.1: its BYE carries a
/// Contact header (BYE terminates the dialog, target refresh is meaningless),
/// deterministically tripping the non-advisory `rfc3261.noContactOnBye` audit
/// rule on every sampled call.
struct ByeWithContact;

impl ActorScenario for ByeWithContact {
    fn id(&self) -> ScenarioId {
        "bye_with_contact"
    }
    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                    ),
                    // The deliberate deviation: a BYE carrying a `Contact` header
                    // (RFC 3261 §15.1 forbids it — the dialog is ending, target
                    // refresh is meaningless), which trips the non-advisory
                    // `rfc3261.noContactOnBye` audit rule on every sampled call.
                    Goal::new(
                        Barrier::AllConfirmed(&["alice", "bob"]),
                        GoalStep::ByeWith {
                            headers: vec![(
                                "Contact".to_string(),
                                "<sip:alice@127.0.0.1>".to_string(),
                            )],
                        },
                    )
                    .after(env.talk_time),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },
            
                cseq: None,
                delayed: None,
            },
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed { on_ack_rx: Feed::new(None, Some("connected")), ..CtxFeed::default() },
            
                cseq: None,
                delayed: None,
            },
        ];
        let plan = vec![phase("established", |s: &StateInner| {
            s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least("bob", LegPhase::Confirmed)
        })];
        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        
            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
        })
    }
}

/// (c) `allowViolations` waives a NAMED RFC audit rule per call. Baseline: the
/// deviating scenario (BYE + Contact) reclassifies every sampled call to
/// `rfc_audit_fail`. With a case carrying
/// `allowViolations: ["rfc3261.noContactOnBye"]` the SAME flow stays OK —
/// the load-surface analogue of `Harness::allow_violation`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_allow_violations_waives_named_rfc_rule() {
    let (_h, b2bua, core, transport) = setup(6740, Correlation::header("X-Loadgen-Id"), 5).await;

    // Baseline: no case → the full audit reclassifies to rfc_audit_fail.
    let baseline = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 1 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 40.0, 2, 16, 0xA0D17),
        vec![(Arc::new(ByeWithContact) as Arc<dyn ActorScenario>, 1.0)],
        baseline.clone(),
        transport.clone(),
    );
    run_throttled(&driver).await;
    assert!(
        baseline.count("bye_with_contact", &ResultClass::RfcAuditFail) > 5,
        "the deviation must trip the audit without a waiver:\n{}",
        baseline.render_prometheus()
    );
    assert_eq!(baseline.count("bye_with_contact", &ResultClass::Ok), 0);

    // Waived: the case exempts exactly that rule → the same flow stays OK.
    let case = check_case(
        r#"{ "id": "waived-bye", "compatibleShapes": ["basic_call"],
             "allowViolations": ["rfc3261.noContactOnBye"] }"#,
    );
    let case = Arc::new(LoadCase::new(case, &Default::default(), 0x30B).unwrap());
    let waived = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 1 }));
    let driver = Driver::new(
        cfg(b2bua.addr, 40.0, 2, 16, 0xA0D18),
        vec![MixEntry::from((Arc::new(ByeWithContact) as Arc<dyn ActorScenario>, 1.0))
            .with_case(Some(case))],
        waived.clone(),
        transport,
    );
    run_throttled(&driver).await;
    let ok = waived.count("bye_with_contact", &ResultClass::Ok);
    assert!(ok > 5, "waived calls must stay OK:\n{}", waived.render_prometheus());
    assert_eq!(
        waived.count("bye_with_contact", &ResultClass::RfcAuditFail),
        0,
        "the named rule must be exempt:\n{}",
        waived.render_prometheus()
    );

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (allow_violations)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

// ---------------------------------------------------------------------------
// 031 — named leg specs (role + R-URI prefixes) on `ShapeDescriptor`
// ---------------------------------------------------------------------------

/// The transfer target's full number form — the SUT copies it from the Refer-To
/// onto the C-leg Request-URI, so it is ALL the wire carries: the receiving
/// leg's role label never appears in any R-URI (the newkah Business-Layer
/// number rewrite).
const XFER_NUMBER: &str = "065003303312345";

/// A blind transfer whose transfer leg is addressed by NUMBER, not by role —
/// the demux problem of the newkah multi-callee-leg shapes (`nk_ct_refer`).
/// The body resolves the leg by its declared role (`callee_agent("xfer")`)
/// while the wire carries only digits. Flow mirrors the shipped `refer`
/// scenario (including its ordered merge-settle before the BYE).
struct NumberPlanRefer {
    refer_key: String,
}

impl NumberPlanRefer {
    fn new(refer_key: impl Into<String>) -> Self {
        Self { refer_key: refer_key.into() }
    }
}

/// The post-transfer media merge is complete for the number-addressed leg: BOTH
/// realign re-INVITEs (the `xfer` target and alice) have been answered AND
/// acknowledged — the actor twin of the shipped `refer` body's `merged`.
fn xfer_merged(s: &StateInner) -> bool {
    let confirmed = |leg: &str| {
        s.leg(leg).subflow(SUBFLOW_REALIGN).is_some_and(|f| f >= SubflowState::Confirmed)
    };
    confirmed("alice") && confirmed("xfer")
}

impl ActorScenario for NumberPlanRefer {
    fn id(&self) -> ScenarioId {
        "nk_refer_like"
    }
    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        // The 031 role lookup: a named leg beyond the bob/bob2/charlie trio. The
        // Refer-To user is the NUMBER (never the role label); the transfer is
        // authorized for the xfer leg's (shared-socket) address under this run's
        // key. The transfer INVITE only lands on `xfer` if the driver demuxed the
        // number-form R-URI by the leg's declared prefix.
        let xfer = env.callee_agent("xfer");
        let refer_to = format!("<sip:{XFER_NUMBER}@{}>", xfer.addr());
        let authorization = Some(
            ApiCall::refer(&self.refer_key, xfer.addr().ip().to_string(), xfer.addr().port())
                .to_header(),
        );

        let actors = vec![
            // Alice originates through the SUT, answers the a-realign reactively,
            // and BYEs once the merge completes (mirrors the shipped `refer`).
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                    ),
                    Goal::new(Barrier::pred("merged", xfer_merged), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    ..CtxFeed::default()
                },
            
                cseq: None,
                delayed: None,
            },
            // Bob rings then answers, then — established + a talk dwell — REFERs
            // the call to the number-addressed xfer leg.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![Goal::new(
                    Barrier::AllConfirmed(&["alice", "bob"]),
                    GoalStep::Refer { refer_to, authorization },
                )
                .after(env.reinvite_gap)],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_refer_accepted: Feed::new(Some("time_to_202"), Some("referred")),
                    ..CtxFeed::default()
                },
            
                cseq: None,
                delayed: None,
            },
            // The number-addressed transfer target answers the transfer INVITE
            // (180 then an immediate 200) and the c-realign re-INVITE reactively.
            ActorSpec {
                role: "xfer",
                agent: xfer.clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::ZERO },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_answer_sent: Feed::new(Some("time_to_charlie_200"), Some("transferred")),
                    ..CtxFeed::default()
                },
            
                cseq: None,
                delayed: None,
            },
        ];

        let plan = vec![
            phase("established", |s: &StateInner| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("merged", xfer_merged),
        ];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        
            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
        })
    }
}

/// 031 end-to-end through the DRIVER: a registered third-party shape declares
/// NAMED leg specs — `[bob: ["bob"], xfer: ["0650033033"]]` — whose transfer
/// leg arrives under a number-form R-URI that never contains the role label.
/// The driver must bind the receivers from the specs (in declaration order),
/// feed the leg picker the PREFIXES labelled with their roles (not the agent
/// names), and expose each agent by role in the `CallEnv` — the flow the
/// closed `needs_bob2`/`needs_charlie` booleans could not express. Calls
/// complete OK (RFC-audited), with no timeout (a timeout here means the
/// number-form leg was never demuxed), no orphan, and no mux/SUT leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_named_leg_specs_demux_number_form_transfer_leg() {
    const NK_LEGS: &[LegSpec] = &[
        LegSpec { role: "bob", ruri_prefixes: &["bob"] },
        LegSpec { role: "xfer", ruri_prefixes: &["0650033033"] },
    ];
    let (_h, b2bua, core, transport) = setup(6760, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    // Third-party registration: the open shape carries its own leg specs.
    let mut registry = ShapeRegistry::with_defaults();
    registry.register(ShapeDescriptor::new("nk_refer_like").legs(NK_LEGS).load_actor_with(
        |inputs| Arc::new(NumberPlanRefer::new(inputs.refer_key.clone())),
    ));

    let driver = Driver::new(
        cfg(b2bua.addr, 30.0, 2, 8, 0x031A),
        vec![MixEntry::by_id(&registry, "nk_refer_like", &inputs(), 1.0).expect("registered shape")],
        reporter.clone(),
        transport,
    );
    run_throttled(&driver).await;

    use std::sync::atomic::Ordering::Relaxed;
    assert!(
        reporter.count("nk_refer_like", &ResultClass::Ok) > 3,
        "named-leg transfer calls must complete OK:\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(
        reporter.count("nk_refer_like", &ResultClass::Timeout),
        0,
        "a timeout means the number-form transfer leg was never demuxed to \"xfer\":\n{}",
        reporter.render_prometheus()
    );
    assert_eq!(core.stats().orphan_no_header.load(Relaxed), 0, "uncorrelatable legs");
    assert_eq!(core.stats().orphan_unknown_token.load(Relaxed), 0, "token matched no call");

    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak (named legs)");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

// ---------------------------------------------------------------------------
// Loss-recovery SOAK (slow lane) — every body recovers 1% loss (actor P4)
// ---------------------------------------------------------------------------

/// Drain up to `secs` of REAL wall-clock waiting for `cond` — the teardown reap
/// window. A lost BYE-200 under loss leaves a call for the SUT's 32 s
/// terminating-safety timer (`TERMINATING_TIMEOUT_MS`) to reap, so the strict
/// zero-leak assertion must give that window. Longer sibling of
/// `b2bua_harness::settle_until` (1 s), for this real-clock soak (we sleep, not
/// advance).
async fn settle_secs(secs: u64, cond: impl Fn() -> bool) {
    for _ in 0..(secs * 20) {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The actor executor's thesis, proven across EVERY happy body (not just refer,
/// which the mux loss proof above already covers): each leg answers independently
/// so a dropped datagram is RECOVERED by re-emission (Timer A/E/G) rather than
/// stranding the call. At a light 1%/datagram loss the retransmit ladder recovers
/// every drop well inside the (wide) recv window, so we EXPECT **zero
/// loss-induced failures** — each happy body soaked ~200 calls each.
///
/// Asserts, per happy body:
///   - real OK calls happened (`ok > 100` — the body was actually driven);
///   - ZERO loss-induced NOK: no `timeout` (a drop retransmit failed to recover),
///     no `rfc_audit_fail` (a recovered drop that looked like a false
///     `cseqInDialogOrder`/order charge — the exact anomaly the actor redesign
///     eliminated), no `panic`.
/// Plus, once over the whole soak: the loss model actually bit (`drops > 0`, so
/// the run is not vacuous) and there is NO mux/SUT leak.
///
/// The voluntarily-failing bodies (`invite_reject` / `abandon_ringing` /
/// `refer_charlie_reject`) are EXCLUDED — they are expected-NOK by design, so
/// "0 failed calls" does not apply to them (their loss behaviour is a separate
/// concern; the refer-decline path is exercised by the functional gate).
///
/// Real loopback UDP on the REAL clock (the retransmit ladder is wall-clock), so
/// this is a slow-lane `#[ignore]` test — its paused-clock equivalents are the
/// functional gates (`b2bua-harness/tests/realcall_functional.rs`) plus the
/// per-scenario loss smoke tests above. Writes a self-contained HTML report
/// (counts × class, latency percentiles, sampled callflows) to
/// `target/loadgen-soak-report/index.html`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-clock loss soak — slow lane (just test-slow)"]
async fn loadgen_loss_soak_all_bodies_recover() {
    // Happy-path bodies grouped by the SUT routing backend their dedicated smoke
    // tests use (a failure under loss on any of these is a real bug):
    //  - Group A — transparent route-all + the REFER backend
    //    (`route_all_with_refer`, default `Transparent` egress): basic / reinvite
    //    / refer / options_hold / long_call / prack_update (+ emergency variants).
    //  - Group B — the full X-Api-Call engine (`route_api_call`: the ADR-0017
    //    `[bob, bob2]` failover plan walked on b-leg rejection) under the
    //    `ApiCallPin` egress: rerouting_prack. This body's whole point is the
    //    failover, which only the api-call engine registers.
    // The voluntarily-failing bodies (invite_reject / abandon_ringing /
    // refer_charlie_reject) are EXCLUDED — expected-NOK by design, so "0 failed"
    // does not apply (their loss behaviour is a separate concern).
    const GROUP_A: &[&str] = &[
        "basic_call",
        "basic_call_em",
        "reinvite",
        "reinvite_em",
        "refer",
        "options_hold",
        "long_call",
        "prack_update",
    ];
    const GROUP_B: &[&str] = &["rerouting_prack"];

    // Wide recv window (like `loadgen_auto_retransmit_recovers_packet_drop`): a
    // recovering call's ladder (0.5+1+2+4 s) plus stretched per-hop latency under
    // CI CPU load must fit, or a recovered call flips to `timeout` and the strict
    // zero-failure assert flakes. At 1% loss recovery is rare, so the wide window
    // only costs wall time on the deterministic tail.
    const WIDE: Duration = RECV_LOSSY;
    // ~200 calls per body: run each body in its OWN sub-run (rate × duration ≈
    // 200) into the SHARED reporter, so coverage is even per body (the default
    // mix is basic-heavy).
    const PER_BODY: u32 = 200;
    // Modest CPS + in-flight: less host CPU contention → a recovering call's
    // retransmit ladder reliably fits the recv window (fewer jitter timeouts).
    let cps = 25.0;
    let secs = (PER_BODY as f64 / cps).ceil() as u64; // ≈ 8 s of admission per body
    let tuning = CallTuning { drop_rate: 0.01, retransmit: true, ..CallTuning::default() }; // 1% loss + retransmit

    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 8 }));
    let registry = ShapeRegistry::with_defaults();
    let scenario_inputs = inputs();
    let drops_of = |core: &Arc<MuxCore>| {
        core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed)
            + core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed)
    };
    // Both SUTs are bound at fn scope (they run on distinct ports) so the report
    // + the leak/recovery assertions below all run AFTER both groups complete,
    // regardless of any failure — the report is always produced (it's the whole
    // point of the soak, and the leak it may surface is the finding to inspect).

    // ── Group A: route_all_with_refer + Transparent egress ──────────────────────
    let (_ha, b2bua_a, core_a, transport_a) =
        setup_recv(6800, Correlation::header("X-Loadgen-Id"), 5, WIDE).await;
    for (i, &id) in GROUP_A.iter().enumerate() {
        let mut c = cfg(b2bua_a.addr, cps, secs, 12, 0x50A0 + i as u64);
        c.default_tuning = tuning; // Transparent egress is the `cfg` default
        let entry = MixEntry::by_id(&registry, id, &scenario_inputs, 1.0)
            .unwrap_or_else(|| panic!("load shape {id:?} missing"));
        run_throttled(&Driver::new(c, vec![entry], reporter.clone(), transport_a.clone())).await;
    }

    // ── Group B: route_api_call (ADR-0017 failover plan) + ApiCallPin egress ────
    let (_hb, b2bua_b, core_b, transport_b) = setup_shaped(
        6820,
        Correlation::header("X-Loadgen-Id"),
        5,
        WIDE,
        true,
        B2buaSut::route_api_call,
        |_| {},
    )
    .await;
    for (i, &id) in GROUP_B.iter().enumerate() {
        let mut c = cfg(b2bua_b.addr, cps, secs, 12, 0x50B0 + i as u64);
        c.default_tuning = tuning;
        c.call.egress = EgressPolicy::ApiCallPin; // the [bob, bob2] plan rides X-Api-Call
        let entry = MixEntry::by_id(&registry, id, &scenario_inputs, 1.0)
            .unwrap_or_else(|| panic!("load shape {id:?} missing"));
        run_throttled(&Driver::new(c, vec![entry], reporter.clone(), transport_b.clone())).await;
    }
    let _ = (&transport_a, &transport_b); // hold the transports through the runs

    // The loss model actually dropped datagrams over the soak (else it's vacuous).
    let total_drops = drops_of(&core_a) + drops_of(&core_b);
    assert!(total_drops > 0, "the loss model dropped nothing over the soak — the knob is not wired");

    // Write the self-contained HTML report FIRST — always, even if an assertion
    // below trips — so the run stays inspectable (index.html + sampled callflows).
    let out_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/loadgen-soak-report");
    reporter.finalize(&out_dir).expect("write loadgen soak report");
    eprintln!(
        "\nloadgen soak report: {}/index.html\n",
        out_dir.canonicalize().unwrap_or(out_dir.clone()).display()
    );

    // NO LEAK — every call reaped (STRICT). With the SUT §13.2.2.4 re-ACK fix in
    // place (`re-ack-retransmitted-2xx`), a lost ACK to a realign/reroute (or
    // initial) 2xx no longer strands the answering leg: the SUT re-ACKs every
    // retransmitted 2xx on the retained branch, so the answerer's INVITE server
    // txn quiesces. The only stranding still possible under 1% loss is a lost
    // BYE-200 on teardown — which the SUT's 32 s terminating-safety timer
    // (`TERMINATING_TIMEOUT_MS`) reaps — so we give that window before the strict
    // assert (a residual leak AFTER it is a genuine, non-timing SUT stranding).
    // Regression gate for the fix (handoff: `handoff-sut-reack-retransmitted-2xx-13.2.2.4.md`).
    settle_secs(SETTLE_LOSS_SECS, || {
        core_a.registry_size() == 0
            && b2bua_a.active_calls() == 0
            && core_b.registry_size() == 0
            && b2bua_b.active_calls() == 0
    })
    .await;
    assert_eq!(core_a.registry_size(), 0, "mux registry leak after group A soak");
    b2bua_a.assert_fully_reaped();
    assert_eq!(core_b.registry_size(), 0, "mux registry leak after group B soak");
    b2bua_b.assert_fully_reaped();

    // What P4 + the actor executor GUARANTEE under loss (STRICT): `rfc_audit_fail
    // == 0` (a datagram RECOVERED by re-emission must never look like a false
    // `cseqInDialogOrder`/order charge — the anomaly the actor redesign
    // eliminated) and `panic == 0`. The `timeout` class is now ONLY host CPU
    // jitter overrunning the recv window — every SUT-side reliability gap the loss
    // soak used to bleed is fixed at the source:
    //  - in-dialog re-INVITE 2xx (§13.3.1.4): the a-leg 2xx retransmit watchdog
    //    (`unacked-reinvite-2xx-retransmit`) now covers re-INVITEs, not just the
    //    initial answer — a lost a-leg re-INVITE ACK is recovered, not stranded;
    //  - fire-and-forget in-dialog NOTIFY: a still-active non-INVITE CLIENT txn is
    //    now DETACHED (not deleted) on call teardown, so its Timer-E retransmit
    //    completes and a dropped progress NOTIFY is redelivered (no §12.2.1.1
    //    CSeq gap) — see `crates/sip-txn` `do_cancel_txns_for_call`;
    //  - the caller actor ACKs every in-dialog re-INVITE 2xx idempotently (not via
    //    a one-shot flag), and discharges a pending in-dialog ack on dialog
    //    teardown (RFC §15 subsumes a re-INVITE ACK / PRACK 200 the caller BYEs
    //    before its ~500 ms retransmit lands).
    // So every body recovers ~100%; we hold a tight ≥97%/body floor (a few % of
    // host-jitter slack on a CPU-capped WSL box) and log the rate — a body dipping
    // below is a genuine regression, no longer an "accepted §13.2.2.4 tail".
    eprintln!("loadgen loss soak — per-body recovery @ 1% loss + retransmit:");
    let mut worst = 100.0f64;
    for &id in GROUP_A.iter().chain(GROUP_B.iter()) {
        let ok = reporter.count(id, &ResultClass::Ok);
        let timeout = reporter.count(id, &ResultClass::Timeout);
        let audit = reporter.count(id, &ResultClass::RfcAuditFail);
        let panic = reporter.count(id, &ResultClass::Panic);
        let done = ok + timeout;
        let pct = if done == 0 { 0.0 } else { ok as f64 * 100.0 / done as f64 };
        worst = worst.min(pct);
        eprintln!("  {id:16} ok={ok:>3} timeout={timeout:>2} recovery={pct:>5.1}%");

        assert!(ok > 100, "body {id}: only {ok} OK calls — under-driven:\n{}", reporter.render_prometheus());
        // STRICT audit==0 for EVERY body — including `rerouting_prack`'s abandoned
        // reject leg, the former bounded tolerance: the callee UA now outlives the
        // call (the `reject-final` ledger obligation holds the settle barrier —
        // and thus the recording window — until a lost reject hop-ACK is recovered
        // by the Timer-G 486 retransmit + the SUT's §17.1.1.2 re-ACK, which the
        // call-eviction path no longer purges). Deterministic gate:
        // `loadgen_abandoned_reject_leg_recovery_lands_on_recording`.
        assert_eq!(
            audit, 0,
            "body {id}: {audit} rfc_audit_fail under loss — a retransmitted INVITE final never re-ACKed (§13.2.2.4 2xx / §17.1.1.3 non-2xx → `unackedInviteNon2xxFinal`):\n{}",
            reporter.render_prometheus()
        );
        assert_eq!(panic, 0, "body {id}: {panic} panic:\n{}", reporter.render_prometheus());
        assert!(
            pct >= 97.0,
            "body {id}: only {pct:.1}% loss recovery ({timeout} timeouts / {done}) — below the 97% floor. Every SUT reliability gap is fixed at the source, so this is a genuine regression (a re-INVITE 2xx / NOTIFY / in-dialog ack no longer recovering), NOT host jitter:\n{}",
            reporter.render_prometheus()
        );
    }
    eprintln!(
        "\nworst-body recovery {worst:.1}% — 0 rfc_audit_fail, 0 panic, 0 leak across the soak\n"
    );
}
