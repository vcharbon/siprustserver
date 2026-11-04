//! A 2xx to this stack's own re-INVITE that reaches it after a BYE ended the
//! dialog, or while that BYE awaits its final (RFC 3261 §13.2.2.4, RFC 5407 §2):
//! it is ACKed under the re-INVITE's CSeq, an offer it makes answered with every
//! stream rejected (RFC 3264 §6), and draws no second BYE. A repeat is re-ACKed
//! with the same ACK. A 2xx whose ACK a relayed INVITE's originator still owes
//! is not this stack's to acknowledge.
//!
//! The stack's own re-INVITE and the leg's release are a service's
//! ([`reinvite_release`]), on an internal event the test sends.

use b2bua_core::turn_driver::{Driver, Session, Step};
use sip_message::Method;

use crate::calls::bridged;
use crate::services::{reinvite_release, reinvites, releases};

/// The bridged call after the service's re-INVITE toward bob, offering
/// `offer` if any, then its release of his dialog: the re-INVITE's CSeq.
fn reinvite_then_bye<'d>(driver: &'d Driver, offer: Option<&str>) -> (Session<'d>, u32) {
    let mut call = bridged(driver);
    call.run([reinvites("b-1", offer)]);
    let reinvite = call.peer("b-1").and_then(|p| p.last_heard().cloned()).expect("the re-INVITE");
    assert_eq!(*reinvite.method(), Method::Invite);
    assert_eq!(!reinvite.body().is_empty(), offer.is_some(), "the re-INVITE's offer, if any");
    call.run([releases("b-1")]);
    let bye = call.peer("b-1").and_then(|p| p.last_heard().cloned()).expect("the BYE");
    assert_eq!(*bye.method(), Method::Bye);
    (call, reinvite.cseq().seq())
}

/// bob's 2xx to the service's re-INVITE.
fn bob_answers_reinvite() -> Step {
    Step::AnswersHeard("b-1", Method::Invite, 1, 200)
}

/// The ACK `turn` sends `leg`, with its CSeq and body.
fn ack_to(turn: &b2bua_core::turn_driver::Turn, leg: &str) -> (u32, String) {
    let acks: Vec<_> =
        turn.requests_to(leg).into_iter().filter(|r| *r.method() == Method::Ack).collect();
    assert_eq!(acks.len(), 1, "one ACK: {:?}", turn.methods_to(leg));
    (acks[0].cseq().seq(), String::from_utf8_lossy(acks[0].body()).to_string())
}

#[test]
fn a_2xx_crossing_our_bye_is_acked_its_offer_rejected() {
    let driver = Driver::new().with_rule(reinvite_release());
    let (mut call, reinvite) = reinvite_then_bye(&driver, None);
    let turn = call.turned(bob_answers_reinvite());
    assert_eq!(turn.rule, Some("ack-2xx-on-ended-dialog"));
    let (cseq, answer) = ack_to(turn, "b-1");
    assert_eq!(cseq, reinvite, "the ACK names the re-INVITE");
    assert!(answer.contains("m=audio 0 "), "the offer answered, its stream rejected: {answer}");
    assert!(
        !turn.methods_to("b-1").iter().any(|(m, _)| m == "BYE"),
        "the dialog's BYE is already sent"
    );
}

#[test]
fn a_2xx_after_our_bye_was_answered_is_acked() {
    let driver = Driver::new().with_rule(reinvite_release());
    let (mut call, reinvite) = reinvite_then_bye(&driver, None);
    call.turned(Step::AnswersLast("b-1", Method::Bye, 200));
    let turn = call.turned(bob_answers_reinvite());
    assert_eq!(turn.rule, Some("ack-2xx-on-ended-dialog"));
    let (cseq, answer) = ack_to(turn, "b-1");
    assert_eq!(cseq, reinvite);
    assert!(answer.contains("m=audio 0 "), "{answer}");
}

#[test]
fn a_repeat_of_that_2xx_is_re_acked_with_the_same_ack() {
    let driver = Driver::new().with_rule(reinvite_release());
    let (mut call, _) = reinvite_then_bye(&driver, None);
    let first = call.turned(bob_answers_reinvite()).wire_to("b-1");
    let repeat = call.turned(bob_answers_reinvite());
    assert_eq!(repeat.rule, Some("re-ack-retransmitted-2xx"));
    assert_eq!(repeat.wire_to("b-1"), first, "the same ACK");
}

#[test]
fn a_2xx_on_a_confirmed_dialog_is_not_this_rules() {
    let driver = Driver::new().with_rule(reinvite_release());
    let mut call = bridged(&driver);
    call.run([reinvites("b-1", None)]);
    let turn = call.turned(bob_answers_reinvite());
    assert_ne!(turn.rule, Some("ack-2xx-on-ended-dialog"));
}

#[test]
fn a_2xx_answering_our_offer_is_acked_bare() {
    let driver = Driver::new().with_rule(reinvite_release());
    let ours = b2bua_core::turn_driver::SDP.replace("bob", "svc");
    let (mut call, _) = reinvite_then_bye(&driver, Some(&ours));
    let turn = call.turned(bob_answers_reinvite());
    assert_eq!(turn.rule, Some("ack-2xx-on-ended-dialog"));
    let (_, answer) = ack_to(turn, "b-1");
    assert!(answer.is_empty(), "the 2xx answered our offer: the ACK is bare: {answer}");
}

/// alice's re-INVITE relayed to bob, then his dialog released: his 2xx is
/// owed alice's ACK, which she still sends.
#[test]
fn a_2xx_whose_relayed_ack_is_still_owed_is_left_to_its_originator() {
    let driver = Driver::new().with_rule(reinvite_release());
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Invite), releases("b-1")]);
    let outcome = call.step(Step::AnswersLast("b-1", Method::Invite, 200));
    assert_ne!(outcome.turn().and_then(|t| t.rule), Some("ack-2xx-on-ended-dialog"));
}

/// `origin`'s `o=` line, its version one past.
fn next_version(origin: &str) -> String {
    let mut fields: Vec<String> = origin.split(' ').map(str::to_string).collect();
    let version: u64 = fields[2].parse().expect("a version");
    fields[2] = (version + 1).to_string();
    format!("\r\no={}\r\n", fields.join(" "))
}

/// The answer the stack composes continues the session it stated on that
/// dialog (RFC 3264 §8): the version after the one its INVITE stated, under
/// the same `o=` identity, never a session of its own minted for the ACK.
#[test]
fn the_rejecting_answer_continues_the_session_the_stack_stated() {
    let driver = Driver::new().with_rule(reinvite_release());
    let (mut call, _) = reinvite_then_bye(&driver, None);
    let leg = call::helpers::find_leg(call.call(), "b-1").expect("bob's leg");
    let stated = leg.sdp_session.sent_origin.clone().expect("the session stated toward bob");
    let turn = call.turned(bob_answers_reinvite());
    let (_, answer) = ack_to(turn, "b-1");
    assert!(answer.contains(&next_version(&stated)), "{stated}: {answer}");
    assert!(answer.contains("m=audio 0 "), "{answer}");
}

/// The same on the caller's leg: the service's offerless re-INVITE toward
/// her crosses her BYE, and the offer her 2xx makes is answered in the
/// session the stack stated on her dialog, never one minted for the ACK.
#[test]
fn on_the_callers_leg_the_rejecting_answer_continues_the_stacks_session() {
    let driver = Driver::new().with_rule(reinvite_release());
    let mut call = bridged(&driver);
    call.run([reinvites("a", None), Step::Sends("a", Method::Bye)]);
    let stated = call.call().a_leg.sdp_session.sent_origin.clone().expect("the session stated");
    let turn = call.turned(Step::AnswersLast("a", Method::Invite, 200));
    assert_eq!(turn.rule, Some("ack-2xx-on-ended-dialog"));
    let (_, answer) = ack_to(turn, "a");
    assert!(answer.contains(&next_version(&stated)), "{stated}: {answer}");
}
