//! A service's HTTP request runs under a timeout no longer than the service
//! HTTP cap (ADR-0057 §2): a longer one is clamped, a shorter one kept.

use b2bua_core::effects::FireAndForgetEffect;

use super::fixtures::*;
use super::*;

fn timeout_sent(asked: Option<u64>) -> Option<u64> {
    let config = B2buaConfig::default();
    let call = test_call();
    let event = info_event();
    let ctx = context(&call, &event, "a", &config);
    let mut turn = scope(1);
    let mut exec = executor(&config, &mut turn);
    let request = RuleAction::ServiceHttpRequest {
        correlation_id: "c1".into(),
        endpoint: "/svc".into(),
        method: "POST".into(),
        headers: Vec::new(),
        body: Vec::new(),
        content_type: None,
        timeout_ms: asked,
    };
    let result = exec.execute(&[request], &call, &ctx);
    result.effects.fire_and_forget.iter().find_map(|e| match e {
        FireAndForgetEffect::ServiceHttpRequest { timeout_ms, .. } => Some(*timeout_ms),
        _ => None,
    })?
}

#[test]
fn a_service_http_timeout_past_the_cap_is_clamped() {
    let cap = B2buaConfig::default().caps.service_http_timeout_ms;
    assert_eq!(timeout_sent(Some(cap * 3)), Some(cap));
    assert_eq!(timeout_sent(Some(1_000)), Some(1_000), "a shorter timeout is kept");
    assert_eq!(timeout_sent(None), None, "the port's default, clamped where it is read");
}
