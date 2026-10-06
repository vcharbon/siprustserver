//! The loadgen driver under From-user correlation and on a caller/callee
//! shared socket, over the FAKE network under a PAUSED clock.
//!
//! The in-process b2bua SUT meets the two endpoint contracts the modes rely
//! on: it sends each new leg to the loadgen socket that originated the call
//! (its route target is that socket), and it keeps the calling party's From URI
//! user on that leg. It relays no loadgen header, so From-user runs get no
//! header cooperation. Every call is fully terminated and RFC-audited (every
//! call recorded), the mux registry drains and the SUT reaps.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut, B2buaSutBuilder};
use layer_harness::TransportKind;
use loadgen::{
    CallConfig, CallTuning, Correlation, Driver, DriverCfg, EgressPolicy, EndpointSpec, LoadCase,
    MixEntry, MuxCore, MuxTransport, Reporter, ReporterCfg, ResultClass, Role, ScenarioInputs,
    ShapeRegistry,
};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

const RECV: Duration = Duration::from_secs(20);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

fn mix(id: &str) -> MixEntry {
    MixEntry::by_id(&ShapeRegistry::with_defaults(), id, &ScenarioInputs::default(), 1.0)
        .unwrap_or_else(|| panic!("unknown load shape {id:?}"))
}

/// A Test case from its JSON `input`/`bindings` members.
fn case(members: &str) -> Arc<LoadCase> {
    let json = format!(r#"{{ "id": "from-user", "compatibleShapes": ["basic_call"], {members} }}"#);
    Arc::new(LoadCase::new(serde_json::from_str(&json).unwrap(), &Default::default(), 7).unwrap())
}

/// A pool of distinct calling numbers, one per call.
fn caller_pool() -> Arc<LoadCase> {
    case(
        r#""bindings": { "mode": "seq", "entries": [
             { "core": { "from": "sip:+1555${seq:6}@pool.example" } } ] }"#,
    )
}

/// How the loadgen sockets are laid out.
#[derive(Clone, Copy)]
enum Layout {
    /// alice, bob and charlie on one address.
    Shared,
    /// alice, bob and charlie on three addresses.
    Distinct,
}

struct Rig {
    h: Harness,
    b2bua: B2buaSut,
    core: Arc<MuxCore>,
    transport: Arc<MuxTransport>,
}

/// The SUT and the mux on one simulated fabric. The SUT's route target is the
/// socket the loadgen's callee legs bind: the caller's own socket in the
/// shared layout. `relay` names the header the SUT relays (`None`: none).
async fn rig(
    base: u16,
    layout: Layout,
    correlation: Correlation,
    make_sut: fn(&str, u16) -> B2buaSutBuilder,
    relay: Option<&'static str>,
) -> Rig {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let clock = Clock::test_at(0);
    let h = Harness::with_network_and_clock(
        "loadgen-from-user",
        sim.clone() as Arc<dyn SignalingNetwork>,
        clock.clone(),
        TransportKind::Fake,
        RECV,
    );
    h.disarm_cseq_gate(); // infra harness; loadgen runs its own per-call audit

    let (uac, uas, refer) = match layout {
        Layout::Shared => (addr(base), addr(base), addr(base)),
        Layout::Distinct => (addr(base), addr(base + 1), addr(base + 2)),
    };
    let b2bua = make_sut("127.0.0.1", uas.port())
        .tune(move |c| c.relay_headers = relay.into_iter().map(str::to_string).collect())
        .start(&h, "b2bua", &format!("127.0.0.1:{}", base + 3))
        .await;

    let specs = match layout {
        Layout::Shared => vec![EndpointSpec { addr: uac, role: Role::Caller }],
        Layout::Distinct => vec![
            EndpointSpec { addr: uac, role: Role::Caller },
            EndpointSpec { addr: uas, role: Role::Callee },
            EndpointSpec { addr: refer, role: Role::Callee },
        ],
    };
    let core =
        MuxCore::bind_on(sim.as_ref(), specs, correlation.clone(), 256, 8, RECV, clock.clone())
            .await
            .unwrap();
    let transport = Arc::new(MuxTransport {
        core: core.clone(),
        uac_addr: uac,
        uas_addr: uas,
        refer_addr: refer,
        correlation,
        recv_timeout: RECV,
        clock,
    });
    Rig { h, b2bua, core, transport }
}

fn cfg(via: SocketAddr, cps: f64, secs: u64, talk_ms: u64, seed: u64) -> DriverCfg {
    DriverCfg {
        cps,
        duration: Duration::from_secs(secs),
        max_in_flight: 16,
        seed,
        call: CallConfig {
            via,
            egress: EgressPolicy::Transparent,
            options_hold: Duration::from_millis(120),
            options_cadence: Duration::from_millis(40),
            ring_delay: Duration::from_millis(0),
            talk_time: Duration::from_millis(talk_ms),
            reinvite_gap: Duration::from_millis(0),
            long_hold: Duration::from_millis(120),
            teardown_quiesce: Duration::from_millis(200),
        },
        default_tuning: CallTuning::default(),
        tuning: std::collections::HashMap::new(),
    }
}

/// Every call recorded, so every call is RFC-audited.
fn reporter() -> Arc<Reporter> {
    Arc::new(Reporter::new(ReporterCfg { sample_cap: 3, background_record_every: 1 }))
}

fn orphans(core: &MuxCore) -> u64 {
    core.stats().orphans_total()
}

/// The `loadgen_inflight` gauge: every started call was recorded as finished.
fn inflight(reporter: &Reporter) -> String {
    reporter
        .render_prometheus()
        .lines()
        .find(|l| l.starts_with("loadgen_inflight "))
        .unwrap_or("loadgen_inflight <absent>")
        .to_string()
}

/// The post-run gates every test shares: no mux registry leak, a fully
/// reaped SUT (after its dead-call detection), and no call left in flight.
async fn settle(rig: &Rig, reporter: &Reporter) {
    settle_until(|| rig.core.registry_size() == 0).await;
    assert_eq!(rig.core.registry_size(), 0, "mux registry leak");
    rig.h.advance(Duration::from_secs(40)).await;
    settle_until(|| rig.b2bua.is_reaped()).await;
    rig.b2bua.assert_fully_reaped();
    assert_eq!(inflight(reporter), "loadgen_inflight 0", "a call never finished");
}

/// Overlapping calls with distinct calling numbers on ONE shared socket, torn
/// down by the caller (basic) or by both sides at once (crossing BYE): the SUT
/// returns every leg to the caller's socket, each leg reaches its own call by
/// its From user, and every call is OK with no orphan.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn from_user_calls_on_one_shared_socket_each_reach_their_own_call() {
    let rig =
        rig(7400, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    // One pool across the mix, so every call draws the next distinct number.
    let pool = caller_pool();
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 20.0, 2, 300, 0xF20A),
        vec![
            mix("basic_call").with_case(Some(pool.clone())),
            mix("crossing_bye").with_case(Some(pool)),
        ],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok)
        + reporter.count("crossing_bye", &ResultClass::Ok);
    assert!(total >= 20, "governor under-delivered: {total}\n{}", reporter.render_prometheus());
    assert_eq!(ok, total, "NOK calls:\n{}", reporter.render_prometheus());
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    assert_eq!(rig.core.stats().token_collision.load(Relaxed), 0);
    settle(&rig, &reporter).await;
}

/// A calling number already held by an in-flight call is refused: the later
/// call is counted `rejected` (never a panic, never a datagram), the call
/// holding the number completes, and once it ends the number is free again.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_calling_number_in_flight_is_refused_and_counted_while_its_call_completes() {
    let rig =
        rig(7410, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let fixed = case(r#""input": { "core": { "from": "sip:+15550123456@pool.example" } }"#);
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 10.0, 2, 450, 0xD0B1),
        vec![mix("basic_call").with_case(Some(fixed))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    let rejected = reporter.count("basic_call", &ResultClass::Rejected);
    let report = reporter.render_prometheus();
    assert!(total >= 15, "governor under-delivered: {total}\n{report}");
    assert!(rejected > 0, "overlapping calls on one number were not refused:\n{report}");
    assert!(ok > 0, "no call holding the number completed:\n{report}");
    assert_eq!(ok + rejected, total, "a class other than ok/rejected:\n{report}");
    // One number and no pool: each rejected call made all 8 draws, each
    // refused as a collision. Exact only on this current-thread (paused) runtime,
    // where the holder cannot release between one call's draws.
    assert_eq!(rig.core.stats().token_collision.load(Relaxed), 8 * rejected);
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    settle(&rig, &reporter).await;
}

/// The reroute shape on one shared socket: the SUT dials the primary, which
/// rejects, then the alternate, each leg a new INVITE to the caller's socket
/// carrying the same calling number. Both reach the same call; every call is
/// OK.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_second_return_leg_of_a_call_reaches_that_call() {
    let rig =
        rig(7420, Layout::Shared, Correlation::from_user(), B2buaSut::route_api_call, None).await;
    let reporter = reporter();
    let mut c = cfg(rig.b2bua.addr, 8.0, 2, 0, 0x4E42);
    c.call.egress = EgressPolicy::ApiCallPin;
    let driver = Driver::new(
        c,
        vec![mix("rerouting_prack").with_case(Some(caller_pool()))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("rerouting_prack", &ResultClass::Ok);
    assert!(total >= 8, "governor under-delivered: {total}\n{}", reporter.render_prometheus());
    assert_eq!(ok, total, "NOK reroute calls:\n{}", reporter.render_prometheus());
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    settle(&rig, &reporter).await;
}

/// A resolved From whose URI names no user leaves the call without a key: it
/// is counted `rejected` and nothing reaches the SUT.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_userless_from_is_rejected_before_any_datagram() {
    let rig =
        rig(7430, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let userless = case(r#""input": { "core": { "from": "sip:pool.example" } }"#);
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 5.0, 1, 0, 0x0B0A),
        vec![mix("basic_call").with_case(Some(userless))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    assert!(total >= 3, "the rejected calls were not counted:\n{}", reporter.render_prometheus());
    assert_eq!(reporter.count("basic_call", &ResultClass::Rejected), total);
    assert!(rig.h.wire_entries().is_empty(), "a keyless call reached the SUT");
    settle(&rig, &reporter).await;
}

/// A case that resolves no From at all is rejected the same way.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_case_without_a_from_is_rejected_before_any_datagram() {
    let rig =
        rig(7440, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let fromless = case(r#""input": { "core": { "to": "sip:+15550900@callee.example" } }"#);
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 5.0, 1, 0, 0x0B0B),
        vec![mix("basic_call").with_case(Some(fromless))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    assert!(total >= 3, "the rejected calls were not counted:\n{}", reporter.render_prometheus());
    assert_eq!(reporter.count("basic_call", &ResultClass::Rejected), total);
    assert!(rig.h.wire_entries().is_empty(), "a keyless call reached the SUT");
    settle(&rig, &reporter).await;
}

/// From-user correlation on three distinct sockets: the SUT routes the leg to
/// the callee socket and the calling number alone correlates it.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn from_user_correlates_on_distinct_sockets() {
    let rig =
        rig(7450, Layout::Distinct, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 10.0, 2, 200, 0xD157),
        vec![mix("basic_call").with_case(Some(caller_pool()))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    assert!(total >= 10, "governor under-delivered: {total}\n{}", reporter.render_prometheus());
    assert_eq!(ok, total, "NOK calls:\n{}", reporter.render_prometheus());
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    settle(&rig, &reporter).await;
}

/// The shared socket is independent of the correlation: a relayed header
/// correlates the returned legs on one shared socket just as well.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_shared_socket_works_with_header_correlation() {
    let rig = rig(
        7460,
        Layout::Shared,
        Correlation::header("X-Loadgen-Id"),
        B2buaSut::route_all_with_refer,
        Some("X-Loadgen-Id"),
    )
    .await;
    let reporter = reporter();
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 10.0, 2, 200, 0x5A4E),
        vec![mix("basic_call")],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let ok = reporter.count("basic_call", &ResultClass::Ok);
    assert!(total >= 10, "governor under-delivered: {total}\n{}", reporter.render_prometheus());
    assert_eq!(ok, total, "NOK calls:\n{}", reporter.render_prometheus());
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    settle(&rig, &reporter).await;
}

/// A load body whose build panics, or fails without sending anything.
struct Broken {
    panics: bool,
}

impl scenario_harness::actor::ActorScenario for Broken {
    fn id(&self) -> &'static str {
        if self.panics {
            "panicking_build"
        } else {
            "failing_build"
        }
    }
    fn build(
        &self,
        _env: &loadgen::CallEnv<'_>,
    ) -> Result<scenario_harness::actor::ActorCall, scenario_harness::StepError> {
        if self.panics {
            panic!("the load body's build panicked");
        }
        Err(scenario_harness::StepError::Timeout { who: "alice".to_string() })
    }
}

fn broken(panics: bool) -> MixEntry {
    let body: Arc<dyn scenario_harness::actor::ActorScenario> = Arc::new(Broken { panics });
    MixEntry::from((body, 1.0))
}

/// A load body whose build panics is a counted `panic`, and the call still
/// closes its in-flight count.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_panicking_build_is_a_counted_panic() {
    let rig = rig(
        7470,
        Layout::Distinct,
        Correlation::header("X-Loadgen-Id"),
        B2buaSut::route_all_with_refer,
        Some("X-Loadgen-Id"),
    )
    .await;
    let reporter = reporter();
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 5.0, 1, 0, 0xBAD1),
        vec![broken(true)],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    assert!(total >= 3, "panicking builds were not counted:\n{}", reporter.render_prometheus());
    assert_eq!(reporter.count("panicking_build", &ResultClass::Panic), total);
    settle(&rig, &reporter).await;
}

/// A calling number whose call ended NOT ok may still have a leg outstanding
/// at the SUT, so it is held back: a call drawing it within 64·T1 is
/// `rejected` (key cooling) before any datagram.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_calling_number_whose_call_failed_is_held_back() {
    let rig =
        rig(7480, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let fixed = case(r#""input": { "core": { "from": "sip:+15550123456@pool.example" } }"#);
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 2.0, 3, 0, 0xC001),
        vec![broken(false).with_case(Some(fixed))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let report = reporter.render_prometheus();
    assert!(total >= 4, "governor under-delivered: {total}\n{report}");
    assert_eq!(reporter.count("failing_build", &ResultClass::Timeout), 1, "{report}");
    assert_eq!(reporter.count("failing_build", &ResultClass::Rejected), total - 1, "{report}");
    settle(&rig, &reporter).await;
}

/// A bind the layout cannot satisfy (no mux endpoint at the callee address) is
/// a configuration defect: it stays loud as a counted `panic`, never a
/// `rejected` call.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bind_on_an_undefined_endpoint_is_a_counted_panic_not_a_rejection() {
    let rig = rig(
        7490,
        Layout::Distinct,
        Correlation::header("X-Loadgen-Id"),
        B2buaSut::route_all_with_refer,
        Some("X-Loadgen-Id"),
    )
    .await;
    let transport = Arc::new(MuxTransport {
        core: rig.core.clone(),
        uac_addr: rig.transport.uac_addr,
        uas_addr: addr(7499),
        refer_addr: rig.transport.refer_addr,
        correlation: rig.transport.correlation.clone(),
        recv_timeout: RECV,
        clock: rig.transport.clock.clone(),
    });
    let reporter = reporter();
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 5.0, 1, 0, 0xC0F1),
        vec![mix("basic_call")],
        reporter.clone(),
        transport,
    );
    driver.run().await;

    let total = reporter.total_calls();
    let report = reporter.render_prometheus();
    assert!(total >= 3, "calls on an undefined endpoint were not counted:\n{report}");
    assert_eq!(reporter.count("basic_call", &ResultClass::Rejected), 0, "{report}");
    assert_eq!(reporter.count("basic_call", &ResultClass::Panic), total, "{report}");
    settle(&rig, &reporter).await;
}

/// A load body whose FIRST call fails without sending anything and whose
/// later calls are a plain basic call.
struct FailsOnce {
    failed: std::sync::atomic::AtomicBool,
}

impl scenario_harness::actor::ActorScenario for FailsOnce {
    fn id(&self) -> &'static str {
        "fails_once"
    }
    fn build(
        &self,
        env: &loadgen::CallEnv<'_>,
    ) -> Result<scenario_harness::actor::ActorCall, scenario_harness::StepError> {
        if !self.failed.swap(true, Relaxed) {
            return Err(scenario_harness::StepError::Timeout { who: "alice".to_string() });
        }
        scenario_harness::actor::scenarios::BasicCall.build(env)
    }
}

fn fails_once() -> MixEntry {
    let body: Arc<dyn scenario_harness::actor::ActorScenario> =
        Arc::new(FailsOnce { failed: std::sync::atomic::AtomicBool::new(false) });
    MixEntry::from((body, 1.0))
}

/// A number that is cooling is not a failure of the next call: the call
/// re-draws from the case's pool and runs on a free number.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_cooling_number_is_redrawn_from_the_pool() {
    let rig =
        rig(7500, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let pool = case(
        r#""bindings": { "mode": "seq", "entries": [
             { "core": { "from": "sip:+15550700@pool.example" } },
             { "core": { "from": "sip:+15550701@pool.example" } } ] }"#,
    );
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 2.0, 3, 0, 0x4ED4),
        vec![fails_once().with_case(Some(pool))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let total = reporter.total_calls();
    let report = reporter.render_prometheus();
    assert!(total >= 5, "governor under-delivered: {total}\n{report}");
    assert_eq!(reporter.count("fails_once", &ResultClass::Timeout), 1, "{report}");
    assert_eq!(reporter.count("fails_once", &ResultClass::Rejected), 0, "{report}");
    assert_eq!(reporter.count("fails_once", &ResultClass::Ok), total - 1, "{report}");
    assert!(rig.core.stats().key_cooling.load(Relaxed) > 0, "no draw met the cooling number");
    settle(&rig, &reporter).await;
}

/// A call refused because its only number is cooling after a failure near a
/// fault is chaos collateral like the failure itself: tagged `near`.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_cooling_rejection_near_a_fault_is_excused() {
    let rig =
        rig(7510, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let chaos = Arc::new(loadgen::ChaosLog::new(rig.transport.clock.clone()));
    chaos.record("kill_worker", None);
    let fixed = case(r#""input": { "core": { "from": "sip:+15550123456@pool.example" } }"#);
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 2.0, 3, 0, 0xC4A0),
        vec![broken(false).with_case(Some(fixed))],
        reporter.clone(),
        rig.transport.clone(),
    )
    .with_chaos(chaos);
    driver.run().await;

    let rejected = reporter.count("failing_build", &ResultClass::Rejected);
    let report = reporter.render_prometheus();
    assert!(rejected >= 3, "the cooling number was not refused:\n{report}");
    assert_eq!(
        reporter.count_tagged("failing_build", &ResultClass::Rejected, loadgen::ChaosTag::Near),
        rejected,
        "a cooling rejection within the fault's hold must be excused:\n{report}"
    );
    settle(&rig, &reporter).await;
}

/// A cooled number is usable again once `RELEASE_HOLD` has passed, and a call
/// that ended ok leaves its number free for the very next call.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_cooled_calling_number_is_usable_again_after_the_hold() {
    let rig =
        rig(7520, Layout::Shared, Correlation::from_user(), B2buaSut::route_all_with_refer, None)
            .await;
    let reporter = reporter();
    let fixed = case(r#""input": { "core": { "from": "sip:+15550123457@pool.example" } }"#);
    // One call every 10 s for 60 s: the failure at 0 s cools the number past
    // the calls at 10, 20 and 30 s (64·T1 = 32 s); the calls from 40 s on run.
    let driver = Driver::new(
        cfg(rig.b2bua.addr, 0.1, 60, 0, 0x4E1D),
        vec![fails_once().with_case(Some(fixed))],
        reporter.clone(),
        rig.transport.clone(),
    );
    driver.run().await;

    let report = reporter.render_prometheus();
    let total = reporter.total_calls();
    let ok = reporter.count("fails_once", &ResultClass::Ok);
    let rejected = reporter.count("fails_once", &ResultClass::Rejected);
    assert_eq!(reporter.count("fails_once", &ResultClass::Timeout), 1, "{report}");
    assert_eq!(rejected, 3, "the calls inside the hold are refused:\n{report}");
    assert!(ok >= 2, "the number is reused after the hold, then again after an ok call:\n{report}");
    assert_eq!(ok + rejected + 1, total, "{report}");
    settle(&rig, &reporter).await;
}
