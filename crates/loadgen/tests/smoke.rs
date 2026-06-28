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
use loadgen::scenarios::{establish, LoadScenario, ScenarioId};
use loadgen::{
    by_id, default_scenarios, failure_scenarios, CallConfig, CallCtx, CallEnv, CallRouting,
    CallScope, Correlation, Driver, DriverCfg, EndpointSpec, LegInfo, MuxCore, MuxTransport,
    ResultClass, Reporter, ReporterCfg, Role,
};
use scenario_harness::{Harness, StepError};
use sip_clock::Clock;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use tokio::net::UdpSocket;

const RECV: Duration = Duration::from_secs(2);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// Stand up a real-network b2bua SUT + a mux core over a port base. The b2bua
/// routes the b-leg to the static `uas` endpoint (base+1).
async fn setup(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_with(base, correlation, sample_cap, |_| {}).await
}

/// `setup` with an extra `B2buaConfig` mutator (e.g. to exhaust the CPS bucket
/// for an overload-shed test). The relay-header tune is always applied first.
async fn setup_with(
    base: u16,
    correlation: Correlation,
    sample_cap: u32,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let net: Arc<dyn SignalingNetwork> = Arc::new(RealSignalingNetwork::new());
    let h = Harness::with_network_and_clock(
        "mux-smoke",
        net,
        Clock::system(),
        TransportKind::Live,
        RECV,
    );
    h.disarm_cseq_gate(); // infra harness; loadgen runs its own per-call audit

    let (uac, uas, refer) = (base, base + 1, base + 2);
    // Make the in-process b2bua transparent to the loadgen correlation header
    // (the production `B2BUA_RELAY_HEADERS=X-Loadgen-Id`), so the token alice
    // stamps reaches BOTH the b-leg (bob) and the REFER transfer leg (charlie).
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", uas)
        .tune(move |c| {
            c.relay_headers = vec!["X-Loadgen-Id".to_string()];
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
        RECV,
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
    });
    (h, b2bua, core, transport)
}

fn cfg(via: SocketAddr, refer_pin: Option<SocketAddr>, cps: f64, secs: u64, mif: usize, seed: u64) -> DriverCfg {
    DriverCfg {
        cps,
        duration: Duration::from_secs(secs),
        max_in_flight: mif,
        seed,
        call: CallConfig {
            via,
            route_pin: None, // route_all_* routes the b-leg by config, no pin needed
            refer_pin,
            refer_key: "refer-allow-c".to_string(),
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
        cfg(b2bua.addr, None, 60.0, 2, 16, 0xB451C),
        vec![(by_id("basic_call").unwrap(), 1.0)],
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
    assert!(out.join("callflows/basic_call/ok/0.html").exists(), "no OK callflow HTML");
    let _ = std::fs::remove_dir_all(&out);
}

/// All four scenarios (refer last) through the mux → each produces OK, no leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_all_scenarios() {
    let (_h, b2bua, core, transport) = setup(6410, Correlation::header("X-Loadgen-Id"), 5).await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 4 }));
    let refer_pin = Some(transport.refer_addr);

    let driver = Driver::new(
        cfg(b2bua.addr, refer_pin, 60.0, 4, 8, 0xA11),
        default_scenarios(),
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

    let mut cfg = cfg(b2bua.addr, None, 40.0, 2, 16, 0x71A1ED);
    cfg.call.ring_delay = Duration::from_millis(30);
    cfg.call.talk_time = Duration::from_millis(30);
    cfg.call.reinvite_gap = Duration::from_millis(20);
    cfg.call.long_hold = Duration::from_millis(150);

    let driver = Driver::new(
        cfg,
        vec![
            (by_id("basic_call").unwrap(), 2.0),
            (by_id("reinvite").unwrap(), 1.0),
            (by_id("long_call").unwrap(), 1.0),
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
        cfg(b2bua.addr, None, 40.0, 2, 8, 0xDEAD),
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
        |c| {
            c.cps_bucket_size = 0; // exhausted bucket → shed every non-emergency
            c.cps_bucket_rate = 0; // …and never refill (deterministic).
        },
    )
    .await;
    let reporter = Arc::new(Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 2 }));

    // Mix sheddable non-emergency basic calls with force-admitted emergency ones.
    let driver = Driver::new(
        cfg(b2bua.addr, None, 60.0, 3, 16, 0xE5E7),
        vec![
            (by_id("basic_call").unwrap(), 1.0),
            (by_id("basic_call_em").unwrap(), 1.0),
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
    let refer_pin = Some(transport.refer_addr);

    let mut scenarios = failure_scenarios();
    scenarios.push((by_id("basic_call").unwrap(), 2.0)); // some happy traffic in the mix
    let driver = Driver::new(cfg(b2bua.addr, refer_pin, 50.0, 3, 12, 0xFA17), scenarios, reporter.clone(), transport);
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
