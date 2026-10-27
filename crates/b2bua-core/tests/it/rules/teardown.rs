//! The caller's BYE and the call's teardown as the rules read them: an early
//! dialog's BYE answered with its INVITE's 487, an ended leg's BYE, and a
//! termination that sends the caller nothing her INVITE's final rules out.

use b2bua_core::turn_driver::{Driver, Step};

use super::fixtures::*;
use super::*;

/// A callee's 2xx relayed toward the caller lost the race to her CANCEL: the
/// transaction layer had already answered her INVITE 487 (RFC 3261 §9.2), so
/// the 2xx never left and she holds no dialog. The `Cancelled` turn records
/// the 487 over the record's 2xx, and the termination sends her no BYE
/// (§15: a BYE names a dialog that exists).
#[test]
fn a_2xx_the_cancel_beat_to_the_caller_leaves_her_no_dialog_to_bye() {
    let driver = Driver::new();
    let mut call = crate::calls::ringing(&driver);
    call.run([Step::Answers("b-1", 200)]);
    assert_eq!(call.call().a_leg.invite_final_sent(), Some(200), "the record's 2xx");
    let turn = call.turned(Step::AliceCancels);
    let to_a = turn.wire_to("a");
    assert!(to_a.is_empty(), "nothing goes to the caller, got {} messages", to_a.len());
    let record = call.call();
    assert_eq!(record.a_leg.invite_final_sent(), Some(487), "the wire's final is the layer's 487");
    assert_eq!(record.a_leg.state(), LegState::Terminated);
}

/// A caller's BYE while its INVITE is still pending (RFC 3261 §15: the early
/// dialog bob's relayed 180 opened; the a-leg itself reads Trying until its
/// answer) is `early-dialog-bye`'s, never `relay-bye`'s: the BYE is answered
/// 200, the pending INVITE 487 (§15.1.2), and the call terminates as a caller
/// release. A BYE on an answered call is not this rule's.
#[test]
fn bye_on_the_early_a_dialog_selects_early_dialog_bye_with_487_and_termination() {
    let driver = Driver::new();
    let mut call = crate::calls::ringing(&driver);
    let turn = call.turned(Step::Sends("a", sip_message::Method::Bye));
    assert_eq!(turn.rule, Some("early-dialog-bye"));
    let answers: Vec<(u16, String)> = turn
        .responses_to("a")
        .iter()
        .map(|r| (r.status(), r.cseq().method().as_str().to_string()))
        .collect();
    assert_eq!(
        answers,
        [(200, "BYE".to_string()), (487, "INVITE".to_string())],
        "200 to the BYE, then 487 to the INVITE"
    );
    let termination = call.call().termination.clone().expect("the call terminates");
    assert_eq!(termination.cause, TerminationCause::RemoteBye);
    assert_eq!(termination.by_leg.as_deref(), Some("a"), "a caller release");

    let mut answered = crate::calls::bridged(&driver);
    let turn = answered.turned(Step::Sends("a", sip_message::Method::Bye));
    assert_ne!(turn.rule, Some("early-dialog-bye"), "an INVITE with its final draws no second");
}

/// The same BYE on a call already terminating (a b-leg's BYE ended it while
/// the caller still rang, so the termination stamped the early a-leg's BYE
/// disposition): still `early-dialog-bye`, never `resolve-ended-leg-bye`,
/// which would answer the BYE alone; the two finals go out, and the
/// teardown under way is not restarted.
#[test]
fn bye_on_the_early_a_dialog_of_a_terminating_call_still_answers_the_invite() {
    let mut call = call_with_early_a_dialog("btag");
    call.state = CallModelState::Terminating;
    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::sip(SipMessage::Request(bye), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("early-dialog-bye"));
    let actions = (ranked[0].handle)(&ctx).expect("the rule handles the BYE").actions;
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::RespondToALeg { status: 487, .. })),
        "the pending INVITE gets its 487, got {actions:?}",
    );
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "a terminating call is not torn down twice, got {actions:?}",
    );
}

/// A BYE from a leg this stack already ended (Terminated, its own BYE sent)
/// on a call that stays Active selects `resolve-ended-leg-bye`, never
/// `relay-bye`: the leg is no longer a party of the session, so its crossing
/// BYE (RFC 3261 §15.1.2) ends that dialog alone and tears nothing down.
#[test]
fn bye_from_an_ended_leg_selects_resolve_ended_leg_bye_without_termination() {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut ended = b_leg_pending();
    call::testkit::stage_leg(&mut ended, LegState::Confirmed);
    end_dialogs(&mut ended, DialogState::Ending(EndingBy::Bye));
    call = call::helpers::add_b_leg(call, ended);
    assert_eq!(call.state, CallModelState::Active, "only one leg was released");

    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::sip(SipMessage::Request(bye), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("resolve-ended-leg-bye"));
    let actions = (ranked[0].handle)(&ctx).expect("the rule handles the BYE").actions;
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "an ended leg's BYE tears nothing down, got {actions:?}",
    );
}

/// A 481 to this stack's own BYE on a leg already resolved by the peer's
/// crossing BYE (`ByeReceived`) selects no rule on an Active call: the BYE
/// final denies nothing the BYE has not already ended (RFC 3261 §15.1.2), so
/// `handle-481` declines it and the session stays up.
#[test]
fn bye_481_on_a_bye_received_leg_selects_no_teardown() {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut ended = b_leg_pending();
    call::testkit::stage_leg(&mut ended, LegState::Confirmed);
    end_dialogs(&mut ended, DialogState::Ended(call::DialogEnd::PeerBye));
    call = call::helpers::add_b_leg(call, ended);
    assert_eq!(call.state, CallModelState::Active, "only one leg was released");

    let raw = "SIP/2.0 481 Call/Transaction Does Not Exist\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert!(
        !ranked.iter().any(|r| r.id == "handle-481"),
        "a BYE's 481 is never the peer denying the dialog, got {:?}",
        ranked.iter().map(|r| r.id).collect::<Vec<_>>(),
    );
    for rule in &ranked {
        if let Some(outcome) = (rule.handle)(&ctx) {
            assert!(
                !outcome.actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
                "{} tears the session down on a BYE's 481: {:?}",
                rule.id,
                outcome.actions,
            );
        }
    }
}

#[test]
fn begin_termination_resolves_an_a_leg_whose_invite_carries_its_final() {
    // The `Cancelled` turn: sip-txn sent the 487 and the record carries that
    // final, so the a-leg reads Terminated — resolved, never re-answered —
    // and `BeginTermination` leaves it so.
    let mut call = test_call();
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    let call = call::helpers::record_invite_final(call, "a", 487);
    assert_eq!(call.a_leg.state(), LegState::Terminated);
    let result = execute_on(
        &call,
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
    );
    assert_eq!(result.call.a_leg.state(), LegState::Terminated);
    assert_eq!(result.call.a_leg.end(), Some(call::ByeDisposition::Rejected));
    assert!(result.call.a_leg.is_resolved());

    // An a-leg with NO final yet stays Early: the ADR-0022 funnel still owes
    // that caller its 503 at `→ terminated`.
    let mut call = test_call();
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    let result = execute_on(
        &call,
        &[RuleAction::BeginTermination {
            reason: Some("BYE".into()),
            cause: TerminationCause::RemoteBye,
            by_leg: Some("a".into()),
        }],
    );
    assert_eq!(result.call.a_leg.state(), LegState::Early);
}
