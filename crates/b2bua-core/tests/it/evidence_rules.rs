//! An INVITE final on a confirmed dialog, as the default rules read it. One
//! whose CSeq is above the last the record sent on the dialog, no request
//! record answering it, is evidence the record cannot explain (ADR-0055 §4):
//! it is ACKed — a 2xx end to end under a raised sequence (RFC 3261
//! §13.2.2.4), a non-2xx hop by hop on its own branch (§17.1.1.3) — and the
//! call is ended. A repeat of that 2xx is re-ACKed with the same ACK. A
//! non-2xx that is no evidence is not read as the confirmed leg failing.
//!
//! The record lacks the re-INVITE: the turn that relayed it landed a write
//! that was lost ([`Session::lose`]), and bob answers it.

use b2bua_core::turn_driver::{Driver, Session, Step};
use call::CallModelState;
use sip_message::{Method, SipRequest};

use crate::calls::bridged;

/// The bridged call whose record lost the turn relaying alice's re-INVITE
/// to bob: the re-INVITE bob heard.
fn reinvite_lost(driver: &Driver) -> (Session<'_>, SipRequest) {
    reinvite_lost_after(driver, Step::Sends("a", Method::Invite))
}

/// [`reinvite_lost`], alice's re-INVITE sent as `reinvite`.
fn reinvite_lost_after(driver: &Driver, reinvite: Step) -> (Session<'_>, SipRequest) {
    let mut call = bridged(driver);
    call.lose([reinvite]);
    let reinvite = call.peer("b-1").and_then(|p| p.last_heard().cloned()).expect("bob's re-INVITE");
    (call, reinvite)
}

#[test]
fn a_2xx_beyond_the_record_is_acked_under_its_cseq_and_the_call_ended() {
    let driver = Driver::new();
    let (mut call, reinvite) = reinvite_lost(&driver);
    let round = reinvite.cseq().seq();
    let turn = call.turned(Step::AnswersSent("b-1", 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    let to_bob = turn.methods_to("b-1");
    assert_eq!(to_bob[0], ("ACK".into(), round), "the ACK names the round");
    assert!(
        to_bob.iter().any(|(m, cseq)| m == "BYE" && *cseq > round),
        "the callee's BYE is numbered past the round: {to_bob:?}",
    );
    assert!(turn.methods_to("a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
    assert_eq!(call.call().state, CallModelState::Terminating);
}

#[test]
fn a_repeat_of_an_evidence_2xx_is_re_acked_with_the_same_ack() {
    let driver = Driver::new();
    let (mut call, _) = reinvite_lost(&driver);
    let first = call.turned(Step::AnswersSent("b-1", 200)).wire_to("b-1")[0].clone();
    let repeat = call.turned(Step::AnswersLast("b-1", Method::Invite, 200));
    assert_eq!(repeat.rule, Some("re-ack-retransmitted-2xx"));
    assert_eq!(repeat.wire_to("b-1"), [first], "the same ACK (§13.2.2.4)");
}

#[test]
fn a_non_2xx_beyond_the_record_is_acked_on_its_own_branch_and_the_call_ended() {
    let driver = Driver::new();
    let (mut call, reinvite) = reinvite_lost(&driver);
    let turn = call.turned(Step::AnswersSent("b-1", 488));
    assert_eq!(turn.rule, Some("evidence-final"));
    let ack = turn.request_to("b-1", "ACK").expect("the ACK");
    assert_eq!(ack.top_via().branch(), reinvite.top_via().branch(), "the response's own branch");
    assert_eq!(ack.cseq().seq(), reinvite.cseq().seq());
    assert_eq!(ack.request_uri().text(), reinvite.request_uri().text(), "where the INVITE went");
    assert_eq!(call.call().state, CallModelState::Terminating);
}

/// A late 488 to a re-INVITE the record holds answered: at the record's own
/// number, so no evidence, and not the confirmed leg failing either.
#[test]
fn a_non_2xx_that_is_no_evidence_is_not_the_confirmed_leg_failing() {
    let driver = Driver::new();
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Invite), Step::AnswersSent("b-1", 200), Step::AliceAcks]);
    let late = call.step(Step::AnswersLast("b-1", Method::Invite, 488));
    let rule = late.turn().and_then(|t| t.rule);
    assert_ne!(rule, Some("route-failure"), "a confirmed leg is not dialling");
    assert_ne!(rule, Some("evidence-final"), "at the record's number: no evidence");
    assert_eq!(call.call().state, CallModelState::Active, "the call stays up");
}

/// The re-INVITE carried no offer (the caller's carried none), and the
/// callee's 2xx makes one: the ACK owes the answer (RFC 3261 §13.2.2.4, RFC
/// 3264 §4), and this stack answers it rejecting every stream (RFC 3264 §6)
/// before the BYE.
#[test]
fn an_offer_in_an_evidence_2xx_is_answered_in_the_ack_every_stream_rejected() {
    let driver = Driver::new();
    let (mut call, _) = reinvite_lost_after(&driver, Step::SendsBare("a", Method::Invite));
    let turn = call.turned(Step::AnswersSent("b-1", 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    let ack = turn.request_to("b-1", "ACK").expect("the ACK");
    let answer = String::from_utf8_lossy(ack.body()).to_string();
    assert!(answer.contains("m=audio 0 "), "every stream rejected: {answer}");
    assert!(turn.methods_to("b-1").iter().any(|(m, _)| m == "BYE"), "then the BYE");
}

/// The callee repeats its non-2xx while the ACK has not reached it (RFC 3261
/// §17.2.1): no transaction here absorbs the repeat, so the record's own ACK is
/// sent again, the same datagram (§17.1.1.2).
#[test]
fn a_repeat_of_an_evidence_non_2xx_is_re_acked_with_the_same_ack() {
    let driver = Driver::new();
    let (mut call, _) = reinvite_lost(&driver);
    let first = call.turned(Step::AnswersSent("b-1", 488)).wire_to("b-1")[0].clone();
    let repeat = call.turned(Step::AnswersLast("b-1", Method::Invite, 488));
    assert_eq!(repeat.rule, Some("re-ack-evidence"));
    assert_eq!(repeat.wire_to("b-1"), [first], "the same ACK");
}
