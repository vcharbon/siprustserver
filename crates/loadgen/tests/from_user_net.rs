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
    let s = core.stats();
    s.orphan_no_header.load(Relaxed)
        + s.orphan_unknown_token.load(Relaxed)
        + s.orphan_stray.load(Relaxed)
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
    assert_eq!(rig.core.stats().token_collision.load(Relaxed), rejected);
    assert_eq!(orphans(&rig.core), 0, "orphans: {:?}", rig.core.stats().samples());
    settle(&rig, &reporter).await;
}

/// The reroute shape on one shared socket: the SUT dials the primary, which
/// rejects, then the alternate, each leg a new INVITE to the caller's socket
/// carrying the same calling number. Both reach the same call; every call is
/// OK.
#[tokio::test(start_paused = true)]
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
