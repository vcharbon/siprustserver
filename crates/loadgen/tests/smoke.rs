//! Mux-transport smoke tests, over **real loopback UDP** (the mux opens real
//! sockets, so the in-process B2buaCore SUT runs on a real-network `Harness`).
//! They validate the whole pipeline deterministically before any cluster:
//! correlation/demux, concurrency without dialog mixing, no registry leak,
//! orphan observability, teardown, and the sampled callflow report.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua_harness::{settle_until, B2buaSut};
use layer_harness::TransportKind;
use loadgen::scenarios::{establish, BasicCall, LoadScenario, ScenarioId};
use loadgen::{
    CallConfig, CallCtx, CallEnv, CallRouting, CallScope, CallTuning, Correlation, Driver,
    DriverCfg, EgressPolicy, EndpointSpec, LegInfo, LoadCase, MixEntry, MuxCore, MuxTransport,
    ResultClass, Reporter, ReporterCfg, Role, ScenarioInputs, ShapeRegistry,
};
use scenario_harness::{Harness, StepError};
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use tokio::net::UdpSocket;

const RECV: Duration = Duration::from_secs(2);

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
    driver.run().await;

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
    driver.run().await;

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
    driver.run().await;

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
    driver.run().await;

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
    driver.run().await;

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
    driver.run().await;

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

/// A scenario that establishes then bails without hanging up — the driver's
/// teardown must release the dialog (no SUT leak) AND the mux entries must be
/// reclaimed (no registry leak).
struct FailMidCall;

#[async_trait]
impl LoadScenario for FailMidCall {
    fn id(&self) -> ScenarioId {
        "fail_mid_call"
    }
    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx) -> Result<(), StepError> {
        let _dialog = establish(env, scope, ctx).await?;
        Err(StepError::Timeout { who: "alice".to_string() })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_teardown_no_leak() {
    let (_h, b2bua, core, transport) = setup(6420, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 4 }));

    let driver = Driver::new(
        cfg(b2bua.addr, 40.0, 2, 8, 0xDEAD),
        vec![(Arc::new(FailMidCall) as Arc<dyn LoadScenario>, 1.0)],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    assert!(reporter.count("fail_mid_call", &ResultClass::Timeout) > 0, "no failed calls recorded");
    settle_until(|| core.registry_size() == 0).await;
    assert_eq!(core.registry_size(), 0, "mux registry leak after teardown");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();
}

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
        let _ = serve_metrics(bind, render, Some(srv_chaos)).await;
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
    driver.run().await;

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
    driver.run().await;

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

/// Baseline: with a lossy fabric and NO auto-retransmit, dropped datagrams break
/// calls — establishing that the loss model actually bites (so the recovery test
/// below is not vacuous) AND that a lost call still tears down with no leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_packet_drop_without_retransmit_breaks_calls() {
    let (_h, b2bua, core, transport) = setup(6480, Correlation::header("X-Loadgen-Id"), 3).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 10.0, 1, 8, 0xD40F);
    // ~12%/datagram loss, no recovery: over a ~10-message call P(all delivered)
    // ≈ 0.9^10 ≈ 0.35, so the majority of calls must fail.
    c.default_tuning = CallTuning { drop_rate: 0.12, retransmit: false };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    driver.run().await;

    let drops = core.stats().dropped_out.load(std::sync::atomic::Ordering::Relaxed)
        + core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed);
    assert!(drops > 0, "the loss model dropped nothing — the knob is not wired");
    assert!(
        reporter.count("basic_call", &ResultClass::Timeout) > 0,
        "no call failed despite {drops} dropped datagrams:\n{}",
        reporter.render_prometheus()
    );

    // The loadgen's OWN mux state is always reclaimed once the call task ends,
    // even for a failed lossy call. (SUT-side reap is NOT asserted here: with no
    // retransmit the teardown BYE/CANCEL can itself be dropped, so the b2bua only
    // reaps on its own transaction timers, well past this settle window — that is
    // the very fragility the retransmit test proves is fixed.)
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
    let (_h, b2bua, core, transport) =
        setup_recv(6490, Correlation::header("X-Loadgen-Id"), 3, Duration::from_secs(6)).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 8 }));

    let mut c = cfg(b2bua.addr, 12.0, 2, 8, 0x5EED);
    c.default_tuning = CallTuning { drop_rate: 0.10, retransmit: true };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    driver.run().await;

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
    c.default_tuning = CallTuning { drop_rate: 0.0, retransmit: true };
    let driver = Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport);
    driver.run().await;

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
        setup_recv(6520, Correlation::header("X-Loadgen-Id"), 5, Duration::from_secs(5)).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 16 }));

    // The default mix (basic-heavy), production loss + retransmit, 25 s at 25 cps.
    let mut c = cfg(b2bua.addr, 25.0, 25, 64, 0xE0D0E);
    c.default_tuning = CallTuning { drop_rate: 0.001, retransmit: true };
    let driver = Driver::new(c, MixEntry::default_mix(&ShapeRegistry::with_defaults(), &inputs()), reporter.clone(), transport);
    driver.run().await;

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

#[async_trait]
impl LoadScenario for ObservedBasic {
    fn id(&self) -> ScenarioId {
        "pooled_basic"
    }
    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx) -> Result<(), scenario_harness::StepError> {
        self.froms
            .lock()
            .unwrap()
            .push(env.core.from.clone().expect("every pool entry sets `from`"));
        self.dwells.lock().unwrap().push((env.ring_delay, env.talk_time));
        BasicCall.run(env, scope, ctx).await
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
    let case = Arc::new(LoadCase::load(&case_path, 0x10AD));

    let froms = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dwells = Arc::new(std::sync::Mutex::new(Vec::new()));
    let scenario: Arc<dyn LoadScenario> =
        Arc::new(ObservedBasic { froms: froms.clone(), dwells: dwells.clone() });

    // Global dwells stay 0 (the `cfg` default), so an observed 25 ms ring /
    // 10 ms talk can ONLY come from the case extras.
    let driver = Driver::new(
        cfg(b2bua.addr, 50.0, 2, 16, 0xB1D5),
        vec![MixEntry::from((scenario, 1.0)).with_case(Some(case))],
        reporter.clone(),
        transport,
    );
    driver.run().await;

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
