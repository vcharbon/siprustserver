//! The final guard on the a-leg: one final per INVITE transaction (RFC 3261
//! §17.2.1), a provisional after it refused, each refusal reported.

use super::fixtures::*;
use super::*;

fn refusals(result: &HandlerResult) -> Vec<(u16, u16)> {
    result
        .effects
        .buffered
        .iter()
        .filter_map(|e| match e {
            BufferedObservabilityEffect::SecondFinalRefused { status, carried } => {
                Some((*status, *carried))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_second_final_to_the_a_leg_is_refused_and_reported() {
    // Two authored finals in one turn (a reject, then a relayed failure): the
    // a-leg seam admits the first, records it, and refuses the second — not
    // built, not pushed — reporting it for the `second_final_refused` count.
    let call = test_call();
    let reject = |status: u16, reason: &str| RuleAction::RespondToALeg {
        status,
        reason: reason.into(),
        header_updates: vec![],
        contacts: vec![],
    };
    let result = execute_on(
        &call,
        &[
            reject(486, "Busy Here"),
            reject(480, "Temporarily Unavailable"),
            RuleAction::RelayFailureToALeg { status: 503, reason: "Service Unavailable".into() },
        ],
    );
    assert_eq!(finals_to_a(&result), vec![486], "exactly the first final leaves");
    assert_eq!(result.call.a_leg.invite_final_sent(), Some(486));
    assert_eq!(
        refusals(&result),
        vec![(480, 486), (503, 486)],
        "every later final is refused against the one the transaction carries"
    );
}

#[test]
fn a_final_after_the_autonomous_487_is_refused() {
    // The `Cancelled` turn recorded sip-txn's 487; a later rule answering the
    // cancelled call (a service timer, a relayed callee failure) is refused.
    let call = call::helpers::record_invite_final(test_call(), "a", 487);
    let result = execute_on(
        &call,
        &[RuleAction::RespondToALeg {
            status: 480,
            reason: "Temporarily Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        }],
    );
    assert!(finals_to_a(&result).is_empty(), "nothing reaches the caller after the 487");
    assert_eq!(refusals(&result), vec![(480, 487)]);
    assert_eq!(result.call.a_leg.invite_final_sent(), Some(487), "the 487 stands");
}

#[test]
fn a_provisional_is_not_refused_by_the_final_guard() {
    // The seam guards finals only; a relayed 18x on an unanswered leg passes
    // and leaves no fact behind.
    let call = test_call();
    let result = execute_on(
        &call,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 183,
            reason: "Session Progress".into(),
            body: None,
            to_tag: None,
            p_early_media: None,
        }],
    );
    let provisionals = result
        .effects
        .outbound
        .iter()
        .filter(
            |e| matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() == 183),
        )
        .count();
    assert_eq!(provisionals, 1, "the 183 leaves: {:?}", result.effects.outbound);
    assert_eq!(result.call.a_leg.invite_final_sent(), None, "a provisional records no final");
    assert!(refusals(&result).is_empty());
    assert!(late_provisional_refusals(&result).is_empty());
}

#[test]
fn a_provisional_after_the_final_is_refused() {
    // The a-leg INVITE transaction sent its 200: a provisional a rule authors
    // toward it afterwards is refused at the seam — never built, never on the
    // wire (RFC 3261 §13.3.1.1 / §17.2.1) — and the answered dialog keeps its
    // tag.
    let call = call::helpers::record_invite_final(test_call(), "a", 200);
    let result = execute_on(
        &call,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 183,
            reason: "Session Progress".into(),
            body: None,
            to_tag: Some("late-tag".into()),
            p_early_media: None,
        }],
    );
    assert!(
        provisionals_to_a(&result).is_empty(),
        "nothing provisional leaves after the final: {:?}",
        result.effects.outbound
    );
    assert!(
        result.call.a_leg.dialogs.iter().all(|d| d.our_tag != "late-tag"),
        "a refused provisional re-tags no dialog",
    );
    assert_eq!(result.call.a_leg.invite_final_sent(), Some(200), "the 200 stands");
    assert_eq!(
        late_provisional_refusals(&result),
        vec![(183, 200)],
        "the refusal is reported against the final the transaction carries"
    );
    assert!(refusals(&result).is_empty(), "a provisional is not a second final");
}
