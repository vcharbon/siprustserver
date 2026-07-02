//! Phase E acceptance (ADR-0019): declarative identity checks evaluated
//! post-call over the recording — passing on BOTH infra shapes, and a
//! deliberately-wrong regex failing identically on both. The verdict depends
//! only on the recorded bytes, never on which infra produced them.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use e2e_core::checks::{self, Bindings, CheckVerdict};
use e2e_core::model::{CheckBlock, Input};
use e2e_core::{
    BasicCall, CallflowShape, EndpointConfig, FakeLsbcB2bua, InfraShape, RealLoopbackDirect,
};
use scenario_harness::RunReport;

fn case_input(ruri_host_port: &str) -> Input {
    serde_json::from_value(serde_json::json!({
        "core": {
            "from": "sip:+33123456789@example.com",
            "to": "sip:+33987654321@example.com",
            "ruri": format!("sip:+33987654321@{ruri_host_port}")
        }
    }))
    .unwrap()
}

/// The shared `invite-identity` blocks: hold on the fake (LB + b2bua SUT) and
/// the real (direct loopback) infra alike — they assert what bob1 *received*.
fn invite_identity_blocks() -> Vec<CheckBlock> {
    serde_json::from_value(serde_json::json!([
        {
            "on": "bob1.initialInvite",
            "checks": [
                { "field": "from.userInfo", "op": "regex", "value": "^\\+33123456789$" },
                { "field": "from.host", "op": "eq", "value": "example.com" },
                { "field": "from.uri", "op": "eq", "value": "${input.from}" },
                { "field": "from.tag", "op": "exists" },
                { "field": "to.userInfo", "op": "regex", "value": "^\\+33987654321$" },
                { "field": "header(Max-Forwards)", "op": "exists" },
                { "field": "header(X-Never-Sent)", "op": "absent" },
                { "field": "pai", "op": "absent" },
                { "field": "body", "op": "regex", "value": "m=audio" },
                { "field": "source.ip", "op": "eq", "value": "${infra.lbVip.ip}" }
            ]
        },
        {
            "on": "alice.answer",
            "checks": [
                { "field": "to.tag", "op": "exists" },
                { "field": "body", "op": "regex", "value": "m=audio" }
            ]
        }
    ]))
    .unwrap()
}

fn wrong_regex_block() -> Vec<CheckBlock> {
    serde_json::from_value(serde_json::json!([
        {
            "on": "bob1.initialInvite",
            "checks": [
                { "field": "from.userInfo", "op": "regex", "value": "^\\+44" }
            ]
        }
    ]))
    .unwrap()
}

fn evaluate(blocks: &[CheckBlock], report: &RunReport, input: &Input, lb_vip: SocketAddr) -> Vec<CheckVerdict> {
    let refs: Vec<&CheckBlock> = blocks.iter().collect();
    checks::evaluate_blocks(&refs, report, &Bindings { input, lb_vip })
}

fn assert_all_passed(verdicts: &[CheckVerdict]) {
    for v in verdicts {
        assert!(v.passed, "{}.{} [{:?}] failed: {} (actual {:?})", v.on, v.field, v.op, v.detail, v.actual);
    }
}

async fn run_fake() -> (RunReport, Input, SocketAddr) {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    let cfg = EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
        egress: None,
    };
    let input = case_input("127.0.0.1:5070");
    let mut rt = FakeLsbcB2bua.build("checks/fake", &cfg).await;
    let lb_vip = rt.lb_vip;
    BasicCall.run(&mut rt, &input).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());
    (report, input, lb_vip)
}

/// `base`: a per-test fixed port base — distinct across the real tests in this
/// binary (they run concurrently) AND from portability.rs's 35060/35070. The
/// recorder keys lanes on the requested addr, so port 0 is not an option.
async fn run_real(base: u16) -> (RunReport, Input, SocketAddr) {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", format!("127.0.0.1:{base}")),
        ("bob1", format!("127.0.0.1:{}", base + 10)),
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
        egress: None,
    };
    let input = case_input(&format!("127.0.0.1:{}", base + 10));
    let mut rt = RealLoopbackDirect.build("checks/real", &cfg).await;
    let lb_vip = rt.lb_vip;
    BasicCall.run(&mut rt, &input).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");
    assert!(report.passed());
    (report, input, lb_vip)
}

/// invite-identity passes over the FAKE infra (through the LB + b2bua SUT).
#[tokio::test(start_paused = true)]
async fn identity_checks_pass_over_fake_infra() {
    let (report, input, lb_vip) = run_fake().await;
    let verdicts = evaluate(&invite_identity_blocks(), &report, &input, lb_vip);
    assert_eq!(verdicts.len(), 12);
    assert_all_passed(&verdicts);
    assert!(checks::all_passed(&verdicts));
}

/// The SAME blocks pass over the REAL infra — portability of the verdict.
#[tokio::test]
async fn identity_checks_pass_over_real_infra() {
    let (report, input, lb_vip) = run_real(36060).await;
    let verdicts = evaluate(&invite_identity_blocks(), &report, &input, lb_vip);
    assert_eq!(verdicts.len(), 12);
    assert_all_passed(&verdicts);
}

/// A deliberately wrong regex fails IDENTICALLY on both infras: same verdict,
/// same extracted actual value.
#[tokio::test(start_paused = true)]
async fn wrong_regex_fails_on_fake_infra() {
    let (report, input, lb_vip) = run_fake().await;
    let verdicts = evaluate(&wrong_regex_block(), &report, &input, lb_vip);
    assert_eq!(verdicts.len(), 1);
    assert!(!verdicts[0].passed);
    assert_eq!(verdicts[0].actual.as_deref(), Some("+33123456789"));
}

#[tokio::test]
async fn wrong_regex_fails_identically_on_real_infra() {
    let (report, input, lb_vip) = run_real(36160).await;
    let verdicts = evaluate(&wrong_regex_block(), &report, &input, lb_vip);
    assert_eq!(verdicts.len(), 1);
    assert!(!verdicts[0].passed);
    assert_eq!(verdicts[0].actual.as_deref(), Some("+33123456789"));
}

/// Resolution failure semantics: an un-tagged anchor fails a mandatory block
/// loudly and skips an `optional` one.
#[tokio::test(start_paused = true)]
async fn unresolved_anchor_fails_unless_optional() {
    let (report, input, lb_vip) = run_fake().await;
    let blocks: Vec<CheckBlock> = serde_json::from_value(serde_json::json!([
        { "on": "bob1.reInvite", "checks": [ { "field": "from.tag", "op": "exists" } ] },
        { "on": "bob1.reInvite", "optional": true,
          "checks": [ { "field": "from.tag", "op": "exists" } ] }
    ]))
    .unwrap();
    let verdicts = evaluate(&blocks, &report, &input, lb_vip);
    assert_eq!(verdicts.len(), 2);
    assert!(!verdicts[0].passed, "mandatory unresolved block must fail");
    assert!(verdicts[0].detail.contains("no anchor"), "{}", verdicts[0].detail);
    assert!(verdicts[1].passed, "optional unresolved block must skip");
    assert!(verdicts[1].detail.contains("skipped"), "{}", verdicts[1].detail);
}

/// An unknown `${…}` binding is a hard failure (typos must not pass).
#[tokio::test(start_paused = true)]
async fn unknown_binding_fails_loudly() {
    let (report, input, lb_vip) = run_fake().await;
    let blocks: Vec<CheckBlock> = serde_json::from_value(serde_json::json!([
        { "on": "bob1.initialInvite",
          "checks": [ { "field": "from.uri", "op": "eq", "value": "${input.frmo}" } ] }
    ]))
    .unwrap();
    let verdicts = evaluate(&blocks, &report, &input, lb_vip);
    assert!(!verdicts[0].passed);
    assert!(verdicts[0].detail.contains("frmo"), "{}", verdicts[0].detail);
}
