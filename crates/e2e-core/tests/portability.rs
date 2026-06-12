//! M1: the `basic-call` Callflow shape runs over an Infra shape via the run-core
//! seam — the foundation of the portability invariant (ADR-0018). Here the fake
//! `FakeLsbcB2bua` infra (in-process LB + b2bua, paused clock); the real
//! external-cluster infra is the same shape body over a different Endpoint config.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use e2e_core::{
    BasicCall, CallflowShape, EndpointConfig, FakeLsbcB2bua, InfraShape, Input, RealLoopbackDirect,
};

fn fake_cfg() -> EndpointConfig {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
    }
}

/// Sanity: default identities (no input overrides) — mirrors the reference
/// `proxy_b2bua` e2e through the run-core abstraction.
#[tokio::test(start_paused = true)]
async fn basic_call_over_fake_infra_default() {
    let mut rt = FakeLsbcB2bua.build("basic-call/fake/default", &fake_cfg()).await;
    BasicCall.run(&mut rt, &Input::default()).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");

    assert!(report.passed(), "run must pass the RFC hard gate");
    let entries = report.entries();
    assert!(
        entries.iter().all(|e| e.delivered),
        "every hop delivered (alice→lb→b2bua→lb→bob1 and back)"
    );
}

/// The Test case drives From / To / R-URI from input data (numbers).
#[tokio::test(start_paused = true)]
async fn basic_call_over_fake_infra_with_input() {
    let input = Input {
        from: Some("sip:+33123456789@example.com".into()),
        to: Some("sip:+33987654321@example.com".into()),
        ruri: Some("sip:+33987654321@127.0.0.1:5070".into()),
    };
    let mut rt = FakeLsbcB2bua.build("basic-call/fake/input", &fake_cfg()).await;
    BasicCall.run(&mut rt, &input).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");

    assert!(report.passed(), "run must pass the RFC hard gate");

    // The From the Test case supplied reached bob1's INVITE (the SUT preserved it).
    let bob1: SocketAddr = "127.0.0.1:5070".parse().unwrap();
    let lb: SocketAddr = "127.0.0.1:5080".parse().unwrap();
    let invite_to_bob1 = report
        .entries()
        .into_iter()
        .find(|e| e.from == lb && e.to == bob1 && String::from_utf8_lossy(&e.raw).starts_with("INVITE"))
        .expect("an INVITE delivered lb→bob1");
    let text = String::from_utf8_lossy(&invite_to_bob1.raw);
    assert!(
        text.contains("+33123456789"),
        "the input From user-part must survive to bob1's INVITE:\n{text}"
    );
}

/// THE PORTABILITY PROOF: the *identical* `basic-call` shape body runs over a
/// REAL infra — `RealSignalingNetwork` + wall clock via `with_network_and_clock`
/// — with no paused runtime. Real loopback UDP, real time. Same shape, different
/// Infra shape (ADR-0018).
#[tokio::test]
async fn basic_call_over_real_infra() {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:35060"),
        ("bob1", "127.0.0.1:35070"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    let cfg = EndpointConfig {
        schema: None,
        infra_shape: "real-loopback-direct".into(),
        roles,
        recv_timeout_ms: 5_000,
        transit_delay_ms: 0,
    };

    let mut rt = RealLoopbackDirect
        .build("basic-call/real", &cfg)
        .await;
    BasicCall.run(&mut rt, &Input::default()).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");

    assert!(report.passed(), "real-transport run must pass the RFC hard gate");
    assert!(
        report.entries().iter().all(|e| e.delivered),
        "every hop delivered over real loopback"
    );
}
