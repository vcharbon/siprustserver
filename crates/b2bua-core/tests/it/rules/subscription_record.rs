//! This stack as the notifier of a subscription (RFC 6665 §4.2.2): once a
//! NOTIFY of its stating `terminated` left, no NOTIFY leaves on that
//! subscription, whoever asks; another subscription of the dialog still
//! notifies.

use super::*;
use b2bua_core::effects::OutboundBody;
use b2bua_core::rule_fixture::{context, executor, Varied};

/// One executor turn at `now_ms` over `call` for `actions`, in the context of
/// an event from the caller's leg.
fn execute_at(call: &call::Call, now_ms: i64, actions: &[RuleAction]) -> HandlerResult {
    let config = B2buaConfig::default();
    let event = info_event();
    let ctx = context(call, &event, "a", &config).at(now_ms);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = now_ms;
    exec.execute(actions, call, &ctx)
}

/// A NOTIFY toward the caller on the subscription `event` names, stating
/// `state`.
fn notify(event: &str, state: &str) -> RuleAction {
    RuleAction::SendNotify {
        leg_id: "a".into(),
        event: event.into(),
        subscription_state: state.into(),
        content_type: Some("message/sipfrag;version=2.0".into()),
        body: b"SIP/2.0 200 OK".to_vec(),
    }
}

/// The NOTIFYs `result` put on the wire.
fn notifies(result: &HandlerResult) -> usize {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| matches!(&e.body, OutboundBody::Request(r) if *r.method() == Method::Notify))
        .count()
}

/// The caller's leg confirmed, a terminating NOTIFY sent on `refer` at 0,
/// and every wait behind it over at `later`.
fn after_terminated() -> (call::Call, i64) {
    let mut call = test_call();
    confirm_a(&mut call);
    let first = execute_at(&call, 0, &[notify("refer", "terminated;reason=noresource")]);
    assert_eq!(notifies(&first), 1, "the terminating NOTIFY leaves");
    (first.call, 10_000)
}

#[test]
fn no_notify_leaves_on_a_subscription_after_its_terminating_one() {
    let (call, later) = after_terminated();
    let result = execute_at(&call, later, &[notify("refer", "active;expires=60")]);
    assert_eq!(notifies(&result), 0, "the subscription is over (RFC 6665 §4.2.2)");
}

#[test]
fn a_second_terminating_notify_on_the_subscription_is_refused() {
    let (call, later) = after_terminated();
    let result = execute_at(&call, later, &[notify("Refer", "terminated")]);
    assert_eq!(notifies(&result), 0, "the event package is matched without case");
}

#[test]
fn another_subscription_of_the_dialog_still_notifies() {
    let (call, later) = after_terminated();
    let result = execute_at(&call, later, &[notify("refer;id=7", "active;expires=60")]);
    assert_eq!(notifies(&result), 1, "a subscription is its event and id (RFC 6665 §4.5)");
}
