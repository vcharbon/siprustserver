//! The paused-clock rig the fake-network tests share: one simulated fabric
//! carrying an in-process b2bua SUT and the loadgen mux endpoints, and the
//! driver settings those tests start from.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::B2buaSut;
use layer_harness::TransportKind;
use loadgen::{
    CallConfig, CallTuning, Correlation, DriverCfg, EgressPolicy, EndpointSpec, MixEntry, MuxCore,
    MuxTransport, Role, ScenarioInputs, ShapeRegistry,
};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

/// Per-recv timeout. Virtual time: a doomed call idles this long on the
/// paused timeline (cheap), a healthy call returns at its datagram. Wide
/// enough to clear the full retransmit ladder (0.5+1+2+4 s, capped at T2)
/// several times over.
pub(crate) const RECV: Duration = Duration::from_secs(20);

pub(crate) fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

pub(crate) fn mix(id: &str, weight: f64) -> MixEntry {
    MixEntry::by_id(&ShapeRegistry::with_defaults(), id, &ScenarioInputs::default(), weight)
        .unwrap_or_else(|| panic!("unknown load shape {id:?}"))
}

/// Stand up the paused-clock rig: one simulated fabric carrying BOTH the
/// in-process b2bua SUT (bound through the recording `Harness`, so `PanicDump`
/// shows the SUT-side wire on failure) and the mux endpoints (bound raw via
/// `bind_on`; the loadgen runs its own per-call recording/audit above the mux).
pub(crate) async fn setup_fake(base: u16) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_fake_shaped(base, B2buaSut::route_all_with_refer, |_| {}).await
}

/// [`setup_fake`] with an extra `B2buaConfig` mutator applied after the
/// relay-header tune (e.g. an empty admission bucket that refuses every
/// non-emergency INVITE with 503).
pub(crate) async fn setup_fake_tuned(
    base: u16,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_fake_shaped(base, B2buaSut::route_all_with_refer, extra_tune).await
}

/// [`setup_fake`] whose SUT honors the full inbound `X-Api-Call` control
/// surface (destination pin + ADR-0017 `routes` failover plan) — the
/// deployed-cluster engine shape the `api-call-pin` egress addresses. The
/// paused-clock reroute tests run over this (the fake-net mirror of the
/// smoke lane's `setup_api_call`).
pub(crate) async fn setup_fake_api_call(
    base: u16,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_fake_shaped(base, B2buaSut::route_api_call, |_| {}).await
}

pub(crate) async fn setup_fake_shaped(
    base: u16,
    make_sut: fn(&str, u16) -> b2bua_harness::B2buaSutBuilder,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    setup_fake_on(SimulatedSignalingNetwork::new(1), base, make_sut, extra_tune).await
}

/// [`setup_fake`] on a fabric whose sends between `base` (the caller) and
/// the SUT `fault` may refuse.
pub(crate) async fn setup_fake_faulty(
    base: u16,
    fault: sip_net::SendFault,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let sim = SimulatedSignalingNetwork::new(1).with_send_fault(fault);
    setup_fake_on(sim, base, B2buaSut::route_all_with_refer, |_| {}).await
}

async fn setup_fake_on(
    sim: SimulatedSignalingNetwork,
    base: u16,
    make_sut: fn(&str, u16) -> b2bua_harness::B2buaSutBuilder,
    extra_tune: impl FnOnce(&mut b2bua_sdk::B2buaConfig) + 'static,
) -> (Harness, B2buaSut, Arc<MuxCore>, Arc<MuxTransport>) {
    let sim = Arc::new(sim);
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
    let b2bua = make_sut("127.0.0.1", uas)
        .tune(move |c| {
            c.relay_headers = vec!["X-Loadgen-Id".to_string()];
            extra_tune(c);
        })
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

pub(crate) fn cfg(via: SocketAddr, cps: f64, secs: u64, mif: usize, seed: u64) -> DriverCfg {
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
        overload_reject: None,
    }
}
