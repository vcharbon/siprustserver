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
    by_id, default_scenarios, CallConfig, CallCtx, CallEnv, CallScope, Correlation, Driver,
    DriverCfg, EndpointSpec, MuxCore, MuxTransport, ResultClass, Reporter, ReporterCfg, Role,
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
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", uas)
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
            teardown_quiesce: Duration::from_millis(200),
        },
    }
}

/// Basic call, CONCURRENT (max_in_flight > 1) through ONE shared uas socket —
/// proves dialogs are demuxed (no mixing) by the To-user correlation token, with
/// no orphans, no registry leak, an OK callflow sample, and a reaped SUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loadgen_mux_smoke_basic_concurrent() {
    let (_h, b2bua, core, transport) = setup(6400, Correlation::b2bua(), 5).await;
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
    let (_h, b2bua, core, transport) = setup(6410, Correlation::b2bua(), 5).await;
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
    let (_h, b2bua, core, transport) = setup(6420, Correlation::b2bua(), 5).await;
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
