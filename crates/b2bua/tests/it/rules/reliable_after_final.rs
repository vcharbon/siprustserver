//! A reliable provisional on a leg ringing under an answered caller: shown to
//! no one, it is the engine's to PRACK (RFC 3262 §4), once.

use b2bua::rules::actions::{Owed, Settle};
use call::helpers::Arrival;

use super::*;

/// One engine turn over `call` for `actions` in the context of `event`
/// arriving from `b-1`: the engine takes a reliable provisional before the
/// rules — a screened one reaches no action — and settles the PRACKs owed
/// after them.
fn engine_turn_from_b1(
    call: &call::Call,
    event: &CallEvent,
    actions: &[RuleAction],
) -> HandlerResult {
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let mut call = call.clone();
    let arrival = exec.take_reliable_provisional(&mut call, event, "b-1");
    let screened = matches!(arrival, Some(Arrival::Repeat | Arrival::OutOfOrder));
    let actions = if screened { &[][..] } else { actions };
    let mut result = execute_from_b1(&call, event, actions);
    exec.settle_owed_pracks(&mut result.call, &mut result.effects, Owed::Call, Settle::TurnEnd);
    result
}

#[test]
fn a_reliable_provisional_under_an_answered_caller_is_acknowledged_by_the_stack() {
    // The same, reliable: the caller never sees the provisional a PRACK would
    // name, so this stack — the ringing leg's UAC — acknowledges it (RFC 3262
    // §4), whatever the rule did: the PRACK is the engine's.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let event = b_leg_ringing_event(true);

    let (_, actions) = handle_from_b1(&call, &event);
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::RelayToPeer { .. })),
        "nothing is relayed toward the answered caller: {actions:?}"
    );
    let result = engine_turn_from_b1(&call, &event, &actions);
    assert_eq!(requests_to(&result, "b-1", "PRACK"), 1, "the reliable 180 is PRACKed by the stack");
    let dialog = result.call.b_legs[0].dialogs.by_tags("svc", "bobtag").expect("its early dialog");
    assert_eq!(dialog.our_cseq, 2, "the PRACK rides the early dialog's own sequence");
}

#[test]
fn a_relayed_reliable_provisional_after_the_final_is_refused_and_acknowledged() {
    // A rule that still relays a reliable 1xx under an answered caller: the
    // seam refuses it toward her, and the ringing leg — which saw no PRACK
    // from anyone — is acknowledged by this stack (RFC 3262 §4), once.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let event = b_leg_ringing_event(true);
    let relay = [RuleAction::RelayToPeer { transform: Default::default() }];

    let result = engine_turn_from_b1(&call, &event, &relay);
    assert!(
        provisionals_to_a(&result).is_empty(),
        "nothing provisional reaches the caller: {:?}",
        result.effects.outbound
    );
    assert_eq!(late_provisional_refusals(&result), vec![(180, 200)]);
    assert_eq!(requests_to(&result, "b-1", "PRACK"), 1, "the reliable 180 is PRACKed by the stack");

    let repeat = engine_turn_from_b1(&result.call, &event, &relay);
    assert_eq!(requests_to(&repeat, "b-1", "PRACK"), 0, "its retransmission draws no second PRACK");
}
