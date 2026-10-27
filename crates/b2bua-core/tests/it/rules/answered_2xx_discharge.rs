//! RFC 3261 §13.3.1.4: the caller's ACK discharges the retained answered 2xx,
//! the engine's before the rules'.

use super::fixtures::*;
use super::*;

/// The caller's ACK discharges the retained 2xx alongside its §13.3.1.4
/// ladder — both timers leave the ledger and the driver, and the bytes can
/// never be sent again, so they stop riding every replication payload for
/// the rest of the call. The rules then see the discharge as a fact.
#[test]
fn caller_ack_discharges_the_retained_answered_2xx() {
    let call = call_with_retained_answered_2xx();
    let (discharged, after, fx) = discharge_ack(&call, "a");
    assert_eq!(discharged, Some(answered_obligation()), "the a-leg ACK names the retained answer");
    assert!(
        after.a_leg.dialogs.confirmed().expect("dialog kept").answered_invite.is_none(),
        "the retained datagram is discharged from the replicated body",
    );
    assert!(
        after.timers.iter().all(|t| t.key.obligation_of().is_none()),
        "both ladder timers leave the ledger, got {:?}",
        after.timers,
    );
    let cancelled: Vec<String> = fx
        .critical
        .iter()
        .filter_map(|e| match e {
            CriticalStateEffect::CancelTimer { key } => Some(format!("{key:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec!["Rung on AckOf2xx:a:btag:1", "RepeatGiveUp on AckOf2xx:a:btag:1"],
        "both ladder timers leave the driver under their keys",
    );
}

/// A b-leg ACK guards nothing on the a-leg: the retained 2xx (still awaiting
/// the CALLER's ACK) stays, exactly like the ladder timers it rides with.
#[test]
fn callee_side_ack_leaves_the_retained_answered_2xx() {
    let call = call_with_retained_answered_2xx();
    let (discharged, after, fx) = discharge_ack(&call, "b-1");
    assert_eq!(discharged, None, "a b-leg ACK must not discharge the caller's obligation");
    assert!(
        after.a_leg.dialogs.confirmed().expect("dialog kept").answered_invite.is_some(),
        "the retained datagram still awaits the caller's ACK",
    );
    assert_eq!(after.timers.len(), call.timers.len(), "the ladder keeps running");
    assert!(fx.critical.is_empty(), "nothing is cancelled, got {:?}", fx.critical);
}

/// `relay-ack` owns none of the ladder plumbing: an a-leg ACK relays alone.
#[test]
fn relay_ack_pushes_no_ladder_plumbing() {
    let call = call_with_retained_answered_2xx();
    let ack = in_dialog_request(Method::Ack);
    let event = CallEvent::sip(SipMessage::Request(ack), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let relay_ack = ranked.iter().find(|r| r.id == "relay-ack").expect("relay-ack is a candidate");
    let actions = (relay_ack.handle)(&ctx).expect("relay-ack handles an ACK").actions;
    assert!(
        actions.iter().all(|a| matches!(a, RuleAction::RelayToPeer { .. })),
        "relay-ack relays and nothing else, got {actions:?}",
    );
}
