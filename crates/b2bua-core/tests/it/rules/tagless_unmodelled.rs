//! A request of a method this stack relays in no dialog, carrying no To-tag,
//! on a call that has dialled no callee yet: the default rules answer it by
//! its method, 489 to a SUBSCRIBE (RFC 6665 §4.2.1.2) and 405 to any other
//! (RFC 3261 §8.2.1), as with a callee leg, never a retry-later 500 a retry
//! cannot turn.

use super::fixtures::*;
use super::*;

/// The caller's request of `method` on her call's Call-ID, with no To-tag,
/// no body and CSeq `2 <method>`.
fn tagless(method: Method) -> SipRequest {
    invite()
        .thaw()
        .with_method(method.clone())
        .set(header::CSeq::new(2, method))
        .push_raw(HeaderName::Event, "dialog")
        .without_body()
        .freeze()
        .expect("a tagless request of the same call is complete")
}

/// The finals the default rules answer the caller's `request` with on a call
/// that holds no callee leg.
fn finals_with_no_callee_leg(request: SipRequest) -> Vec<u16> {
    let call = test_call();
    let event = CallEvent::sip(SipMessage::Request(request), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let ctx = context(&call, &event, "a", &config);
    let result = execute_rules(&default_rules(), &call, &ctx, &mut exec, &ObligationSet::core());
    finals_to_a(&result)
}

#[test]
fn a_tagless_subscribe_before_any_callee_leg_draws_489() {
    assert_eq!(finals_with_no_callee_leg(tagless(Method::Subscribe)), vec![489]);
}

#[test]
fn a_tagless_publish_before_any_callee_leg_draws_405() {
    assert_eq!(finals_with_no_callee_leg(tagless(Method::Publish)), vec![405]);
}
