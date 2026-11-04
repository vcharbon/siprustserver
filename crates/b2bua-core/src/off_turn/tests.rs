use super::*;
use crate::effects::HandlerEffects;
use crate::test_call::{invite, src};
use call::{ConsultAdmits, TimerKey};

fn call() -> Call {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    crate::initial_invite::build_initial_call(&invite("off-turn"), src(), &config, &mut ids, 0)
}

fn http(correlation_id: &str) -> FireAndForgetEffect {
    FireAndForgetEffect::ServiceHttpRequest {
        call_ref: "c".into(),
        correlation_id: correlation_id.into(),
        endpoint: "/ask".into(),
        method: "POST".into(),
        headers: vec![],
        body: b"q".to_vec(),
        content_type: None,
        timeout_ms: None,
    }
}

fn sending(call: Call, fxs: Vec<FireAndForgetEffect>) -> HandlerResult {
    let mut effects = HandlerEffects::new();
    effects.fire_and_forget = fxs;
    HandlerResult { call, effects }
}

fn result_of(topic: &str, payload: serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: "c".into(),
        topic: topic.into(),
        outcome: "ok".into(),
        payload,
        body: Vec::new(),
        incarnation: None,
    }
}

/// Every request a turn sends is on its record; the same request sent again
/// replaces it; an answer takes it off.
#[test]
fn a_sent_request_is_awaited_until_its_answer() {
    let consult = FireAndForgetEffect::FailureAsyncHttp {
        call_ref: "c".into(),
        request: serde_json::json!({"failed_leg_id": "b-1"}),
        limiter: ConsultAdmits { first_change: 7, may_hold: true },
        deadline: Some(7),
    };
    let sent = record(sending(call(), vec![http("a"), http("b"), consult]), 0);
    assert_eq!(sent.call.awaited.len(), 3);
    let again = record(sending(sent.call, vec![http("a")]), 0);
    assert_eq!(again.call.awaited.len(), 3, "the same request replaces itself");

    let mut call = again.call;
    settle(&mut call, &result_of(SERVICE_HTTP_RESULT, serde_json::json!({"correlation_id": "a"})));
    assert_eq!(call.awaited.len(), 2);
    let fold = result_of(FAILURE_TOPIC, serde_json::json!({ CONSULT_CHANGE: 7 }));
    settle(&mut call, &fold);
    assert_eq!(call.awaited.len(), 1);
    let expiry = CallEvent::Timer {
        call_ref: "c".into(),
        key: TimerKey::call(TimerType::ServiceHttpAnswer { correlation_id: "b".into() }),
        generation: 1,
        incarnation: None,
    };
    settle(&mut call, &expiry);
    assert!(call.awaited.is_empty(), "the deadline's expiry answers it too");
}

/// A turn that ends the call awaits nothing.
#[test]
fn an_ended_call_awaits_nothing() {
    let mut ended = call();
    ended.awaited.push(OffTurnRequest::Refer { request: serde_json::json!({}), sent_at_ms: 0 });
    ended.state = CallModelState::Terminated;
    assert!(record(sending(ended, vec![http("a")]), 0).call.awaited.is_empty());
}
