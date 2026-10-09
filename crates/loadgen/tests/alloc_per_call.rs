//! Heap allocations per load call, by shape: the load driver runs calls
//! through an in-process SUT on the simulated network under a paused clock,
//! one thread, so the count is deterministic. The per-call figure is the
//! difference of two runs of different lengths over the difference of their
//! call counts, so setup and the sampled first calls cancel out. It counts the
//! SUT's allocations too: compare figures from the same SUT, never across.
//!
//! A measurement, not a budget: it prints `allocs/call` and `bytes/call` per
//! shape and asserts only that every call ended in its usual class. Run it:
//! `cargo nextest run --workspace -E 'package(=loadgen) & binary(alloc_per_call)' --run-ignored only --no-capture`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alloc_counter::{measure, AllocCost, CountingAlloc};
use b2bua_harness::{settle_until, B2buaSut};
use layer_harness::TransportKind;
use loadgen::{
    CallConfig, CallTuning, Correlation, Driver, DriverCfg, EgressPolicy, EndpointSpec, MixEntry,
    MuxCore, MuxTransport, Reporter, ReporterCfg, Role, ScenarioInputs, ShapeRegistry,
};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const RECV: Duration = Duration::from_secs(20);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// The SUT and the mux endpoints on one simulated fabric.
async fn setup(base: u16) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let clock = Clock::test_at(0);
    let h = Harness::with_network_and_clock(
        "loadgen-alloc-per-call",
        sim.clone() as Arc<dyn SignalingNetwork>,
        clock.clone(),
        TransportKind::Fake,
        RECV,
    );
    h.disarm_cseq_gate();
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
        1,
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

fn cfg(via: SocketAddr, secs: u64) -> DriverCfg {
    DriverCfg {
        cps: 20.0,
        duration: Duration::from_secs(secs),
        max_in_flight: 64,
        seed: 0xA110C,
        call: CallConfig {
            via,
            egress: EgressPolicy::Transparent,
            options_hold: Duration::from_millis(120),
            options_cadence: Duration::from_millis(40),
            ring_delay: Duration::from_millis(500),
            talk_time: Duration::from_millis(0),
            reinvite_gap: Duration::from_millis(0),
            long_hold: Duration::from_millis(120),
            teardown_quiesce: Duration::from_millis(200),
        },
        default_tuning: CallTuning::default(),
        tuning: std::collections::HashMap::new(),
        overload_reject: None,
    }
}

/// One run of `shape` for `secs` virtual seconds on a fresh rig: its
/// allocation cost (driver run only, setup excluded) and its call count.
fn run(shape: &str, base: u16, secs: u64) -> (AllocCost, u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let (h, b2bua, core, transport) = rt.block_on(setup(base));
    // One sampled call per class, then none: the unsampled path dominates.
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 1, background_record_every: 0 }));
    let entry =
        MixEntry::by_id(&ShapeRegistry::with_defaults(), shape, &ScenarioInputs::default(), 1.0)
            .unwrap_or_else(|| panic!("unknown load shape {shape:?}"));
    let driver = Driver::new(cfg(b2bua.addr, secs), vec![entry], reporter.clone(), transport);
    let ((), cost) = measure(|| rt.block_on(driver.run()));
    rt.block_on(async {
        settle_until(|| core.registry_size() == 0).await;
        h.advance(Duration::from_secs(40)).await;
        settle_until(|| b2bua.is_reaped()).await;
        b2bua.assert_fully_reaped();
    });
    let calls = reporter.total_calls();
    let classes = reporter.render_prometheus();
    let nonzero: Vec<&str> = classes
        .lines()
        .filter(|l| l.starts_with("loadgen_calls_total{") && !l.ends_with(" 0"))
        .collect();
    assert_eq!(nonzero.len(), 1, "{shape}: calls ended in more than one class: {nonzero:?}");
    (cost, calls)
}

#[test]
#[ignore = "slow lane: loadgen bench"]
fn loadgen_allocations_per_call() {
    for (i, shape) in ["basic_call", "invite_reject", "abandon_ringing"].into_iter().enumerate() {
        let base = 7600 + 10 * i as u16;
        let (short, n_short) = run(shape, base, 2);
        let (long, n_long) = run(shape, base, 6);
        let n = n_long - n_short;
        assert!(n >= 40, "{shape}: too few extra calls to amortize: {n}");
        let allocs = (long.allocs - short.allocs) as f64 / n as f64;
        let bytes = (long.bytes - short.bytes) as f64 / n as f64;
        println!(
            "alloc-per-call {shape}: {allocs:.1} allocs/call {bytes:.0} bytes/call \
             ({n_short} vs {n_long} calls)"
        );
    }
}
