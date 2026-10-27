//! The evidence rule's triggers beyond an INVITE final (ADR-0055 §4): a final
//! of any method above its dialog's sequence, a final at a number this side
//! re-used, an ACK of an INVITE the record holds unanswered and a 2xx naming a
//! leg the record lacks. Each is owed what
//! RFC 3261 requires of it and ends the call over every dialog the record or
//! the evidence names; none touches a dialog the evidence proves ended.
//!
//! The evidence is what a peer sends after a write the record lost: the
//! session runs on a copy whose writes are lost
//! ([`b2bua_core::turn_driver::Session::lose`]), the peers keeping what they
//! heard.

use b2bua_core::effects::CriticalStateEffect;
use b2bua_core::rules::{Match, RuleAction, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use b2bua_core::turn_driver::{callee_tag, Driver, Session, Step, Turn};
use call::{CallModelState, DialogState, Direction};
use sip_ids::SigningKey;
use sip_message::Method;

use crate::calls::{bridged, ringing};
use crate::services::{early_media, plays_media, reinvite_release, reinvites};

/// `call` after `lost` ran on a copy whose writes were lost.
fn losing<'d>(mut call: Session<'d>, lost: impl IntoIterator<Item = Step>) -> Session<'d> {
    call.lose(lost);
    call
}

/// Whether `turn` relinquishes the caller's INVITE transaction.
fn relinquishes(turn: &Turn) -> bool {
    turn.result
        .effects
        .critical
        .iter()
        .any(|e| matches!(e, CriticalStateEffect::RelinquishAnsweredInvite { .. }))
}

/// The callee answers a BYE the record never saw sent: its dialog is over, so
/// the call ends with a BYE to the caller alone.
#[test]
fn a_bye_final_beyond_the_record_ends_the_call_and_byes_the_other_dialog_alone() {
    let driver = Driver::new();
    let mut call = losing(bridged(&driver), [Step::Sends("a", Method::Bye)]);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Bye, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    assert!(turn.methods_to("b-1").is_empty(), "the BYE's dialog is over");
    assert!(turn.methods_to("a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
    assert_eq!(call.call().state, CallModelState::Terminating);
}

/// A final to an INFO the record never saw sent: the callee's dialog is BYEd
/// past the number the peer saw (RFC 3261 §12.2.1.1), and the caller's too.
#[test]
fn an_info_final_beyond_the_record_byes_every_dialog_past_its_number() {
    let driver = Driver::new();
    let mut call = losing(bridged(&driver), [Step::Sends("a", Method::Info)]);
    let info = call.peer("b-1").and_then(|p| p.last_of(&Method::Info)).expect("bob's INFO");
    let number = info.cseq().seq();
    let turn = call.turned(Step::AnswersLast("b-1", Method::Info, 481));
    assert_eq!(turn.rule, Some("evidence-final"));
    let to_bob = turn.methods_to("b-1");
    assert!(to_bob.iter().any(|(m, c)| m == "BYE" && *c > number), "{to_bob:?}");
    assert!(turn.methods_to("a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
}

/// This side re-used a number another owner sent: a final at that number
/// under another branch than the request the record holds open answers the
/// other owner's request.
#[test]
fn a_final_at_a_reused_number_under_another_branch_is_evidence() {
    let driver = Driver::new();
    let mut call = losing(bridged(&driver), [Step::Sends("a", Method::Info)]);
    call.run([Step::Sends("a", Method::Info)]);
    let heard = call.peer("b-1").expect("bob").heard.clone();
    let infos: Vec<_> = heard.iter().filter(|r| *r.method() == Method::Info).collect();
    assert_eq!(infos[0].cseq().seq(), infos[1].cseq().seq(), "the number re-used");
    assert_ne!(infos[0].top_via().branch(), infos[1].top_via().branch());
    let turn = call.turned(Step::AnswersHeard("b-1", Method::Info, 0, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
}

/// This side re-used the number of a re-INVITE another owner sent, and has
/// sent past it since: the 2xx to the other owner's re-INVITE is ACKed at its
/// own number, the one that 2xx names (RFC 3261 §13.2.2.4), not at this
/// side's latest.
#[test]
fn the_ack_of_a_2xx_at_a_reused_number_takes_that_number() {
    let driver = Driver::new().with_rule(reinvite_release());
    let mut call = losing(bridged(&driver), [Step::Sends("a", Method::Invite)]);
    call.run([reinvites("b-1", None), Step::Sends("a", Method::Info)]);
    let heard = call.peer("b-1").expect("bob").heard.clone();
    let lost = heard.get(1).cloned().expect("the lost re-INVITE");
    let own = heard.get(2).cloned().expect("this side's re-INVITE");
    assert_eq!(own.cseq().seq(), lost.cseq().seq(), "the number re-used");
    let latest = heard.last().expect("the INFO");
    assert!(latest.cseq().seq() > lost.cseq().seq(), "sent past the re-used number");
    let turn = call.turned(Step::AnswersHeard("b-1", Method::Invite, 1, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    let ack = turn.request_to("b-1", "ACK").expect("the 2xx is ACKed");
    assert_eq!(ack.cseq().seq(), lost.cseq().seq(), "at the number the 2xx names");
}

/// A final at or below the sequence that answers no request held open at its
/// number is a stray, not evidence.
#[test]
fn a_stray_final_at_or_below_the_sequence_is_no_evidence() {
    let driver = Driver::new();
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Info), Step::AnswersSent("b-1", 200)]);
    let stray = call.step(Step::AnswersLast("b-1", Method::Info, 200));
    assert_ne!(stray.turn().and_then(|t| t.rule), Some("evidence-final"));
}

/// The caller ACKs a 2xx the record never saw sent, on a call another owner
/// served: her INVITE was answered, so it draws no final here — its
/// transaction is relinquished — and the dialog the ACK names is BYEd under
/// the ACK's tags.
#[test]
fn an_ack_of_the_unanswered_caller_invite_byes_her_dialog_under_its_tags() {
    let driver = Driver::new();
    let mut call = losing(ringing(&driver), [Step::Answers("b-1", 200)]);
    let shown = call.call().a_leg.own_tag.clone().expect("the tag her 180 showed");
    call.copy_served(true);
    let turn = call.turned(Step::AliceAcks);
    assert_eq!(turn.rule, Some("evidence-ack"));
    assert_eq!(call.call().a_leg.invite_final_sent(), Some(200), "answered elsewhere");
    let turn = call.turns().last().expect("the ACK's turn");
    assert!(turn.statuses_to("a").is_empty(), "no final follows the 2xx");
    assert!(relinquishes(turn), "the caller's INVITE transaction is relinquished");
    let bye = turn.request_to("a", "BYE").expect("the caller's BYE");
    assert_eq!(bye.from().tag(), Some(shown.as_str()), "under the tag the ACK names");
    assert_eq!(bye.to().tag(), Some(b2bua_core::turn_driver::ALICE_TAG));
}

/// The caller ACKs a non-2xx the record never saw sent: the ACK rides her
/// INVITE's branch (RFC 3261 §17.1.1.3), so her INVITE was refused and no
/// dialog of hers is confirmed. It draws no final here, its transaction is
/// relinquished, and nothing BYEs her; the callee's ringing INVITE is
/// CANCELled.
#[test]
fn an_ack_of_a_non_2xx_to_the_unanswered_caller_ends_the_call_without_a_bye_to_her() {
    let driver = Driver::new();
    let reject = serde_json::json!({"code": 480, "reason": "Temporarily Unavailable"});
    let refused =
        [Step::Answers("b-1", 486), Step::FailureAnswers { outcome: "reject", payload: reject }];
    let mut call = losing(ringing(&driver), refused);
    call.copy_served(true);
    let turn = call.turned(Step::AliceAcksRefusal);
    assert_eq!(turn.rule, Some("evidence-ack"));
    assert!(turn.statuses_to("a").is_empty(), "no final follows hers");
    assert!(relinquishes(turn), "the caller's INVITE transaction is relinquished");
    let to_her = turn.methods_to("a");
    assert!(to_her.is_empty(), "no request reaches her: {to_her:?}");
    let record = call.call();
    assert!(
        record.a_leg.invite_final_sent().is_some_and(|s| s >= 300),
        "refused elsewhere: {:?}",
        record.a_leg.invite_final_sent()
    );
    let bob = call::helpers::find_leg(record, "b-1").and_then(|l| l.outbound_invite());
    assert!(bob.is_some_and(|i| i.cancel_sent), "bob's ringing INVITE is CANCELled");
}

/// The caller's re-INVITE on a dialog the record holds early, on a call a
/// copy served: a re-INVITE rides a confirmed dialog only, so another owner
/// answered her 2xx. Her INVITE draws no final here — its transaction is
/// relinquished — the re-INVITE draws 487, and the call ends: her dialog
/// BYEd under the re-INVITE's tag, the callee's ringing INVITE CANCELled.
#[test]
fn a_reinvite_on_the_callers_early_dialog_is_evidence_on_a_served_call() {
    let driver = Driver::new();
    let ringing = losing(ringing(&driver), [Step::Answers("b-1", 200), Step::AliceAcks]);
    let mut served = ringing.clone();
    served.copy_served(true);
    let turn = served.turned(Step::Sends("a", Method::Invite));
    assert_eq!(turn.rule, Some("evidence-reinvite"));
    assert_eq!(turn.statuses_to("a"), [487], "the re-INVITE draws 487, her INVITE nothing");
    assert!(relinquishes(turn), "the caller's INVITE transaction is relinquished");
    let to_her = turn.methods_to("a");
    assert!(to_her.iter().any(|(m, _)| m == "BYE"), "her dialog is BYEd: {to_her:?}");
    let bob = call::helpers::find_leg(served.call(), "b-1").and_then(|l| l.outbound_invite());
    assert!(bob.is_some_and(|i| i.cancel_sent), "bob's ringing INVITE is CANCELled");

    let mut single = ringing;
    let outcome = single.step(Step::Sends("a", Method::Invite));
    let rule = outcome.turn().and_then(|t| t.rule);
    assert_ne!(rule, Some("evidence-reinvite"), "on a call one node served, no evidence");
}

/// An ACK of her INVITE naming no dialog proves no answer: no 2xx draws an
/// ACK without the To-tag (RFC 3261 §13.2.2.4).
#[test]
fn a_tagless_ack_of_the_unanswered_caller_invite_is_no_evidence() {
    let driver = Driver::new();
    let mut call = ringing(&driver);
    call.copy_served(true);
    let ack = format!(
        "ACK sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {};branch=z9hG4bK-tagless\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag={}\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        b2bua_core::turn_driver::ALICE,
        b2bua_core::turn_driver::ALICE_TAG,
        b2bua_core::turn_driver::CALL_ID,
    );
    let outcome = call.step(Step::Raw("a", ack));
    assert_ne!(outcome.turn().and_then(|t| t.rule), Some("evidence-ack"));
}

/// A 2xx whose signed via-extension names a leg the record lacks — bob's
/// REFER was allowed, the stack dialled the target on `b-2`, and the record
/// lost those turns: ACKed and BYEd from its own headers, and the call ended.
#[test]
fn a_2xx_naming_a_leg_the_record_lacks_is_acked_and_byed() {
    let driver = Driver::new().with_config(|c| c.cluster_key = Some(key()));
    let transferred = [Step::Sends("b-1", Method::Refer), allowed()];
    let mut call = losing(bridged(&driver), transferred);
    assert!(call::helpers::find_leg(call.call(), "b-2").is_none(), "the record lacks b-2");
    let turn = call.turned(Step::Answers("b-2", 200));
    assert_eq!(turn.rule, Some("evidence-unknown-leg-2xx"));
    let to_target: Vec<String> = turn.methods_to("b-2").into_iter().map(|(m, _)| m).collect();
    assert_eq!(to_target, ["ACK", "BYE"]);
    assert_eq!(call.call().state, CallModelState::Terminating);
}

/// The `/call/refer` consult allowing bob's transfer to carol.
fn allowed() -> Step {
    let target = serde_json::json!({"destination": {"host": "127.0.0.1", "port": 5071}});
    Step::ReferAnswers { outcome: "allow", payload: target }
}

/// The key that signs the stack's via-extensions.
fn key() -> SigningKey {
    SigningKey::new(&[7u8; 32]).expect("a key")
}

/// A failover toward carol.
fn carol() -> serde_json::Value {
    serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": 5071},
        "new_ruri": "sip:carol@127.0.0.1:5071",
        "features": serde_json::to_value(b2bua_core::decision::default_platform_features())
            .unwrap_or_default(),
    })
}

/// A 2xx of a leg the record lacks, on a ringing call whose caller hears a
/// media leg's early media under her primary tag: another owner relayed that
/// 2xx under the tag fronting the callee's dialog, as the primary is the media
/// leg's own session (RFC 3264 §8). The non-2xx she is owed, and the hold,
/// take that fronting tag (ADR-0055 §4). Here bob refused, the failure
/// consult rerouted to carol on `b-3`, and the record lost those turns.
#[test]
fn the_hold_of_a_leg_the_record_lacks_takes_the_fronting_tag_beside_early_media() {
    let [dials, shows] = early_media();
    let driver = Driver::new()
        .with_config(|c| c.cluster_key = Some(key()))
        .with_rule(dials)
        .with_rule(shows);
    let mut call = driver.dial();
    call.run([plays_media(), Step::Answers("b-2", 183)]);
    let primary = call.call().a_leg.own_tag.clone().expect("alice's primary tag");
    let showing = call::helpers::caller_dialog(call.call(), &primary).expect("her primary dialog");
    assert_eq!(showing.shows_media.as_deref(), Some("b-2"), "the media leg's early media");
    let rerouted =
        [Step::Answers("b-1", 486), Step::FailureAnswers { outcome: "failover", payload: carol() }];
    call.lose(rerouted);
    let turn = call.turned(Step::Answers("b-3", 200));
    assert_eq!(turn.rule, Some("evidence-unknown-leg-2xx"));
    let record = call.call();
    let fronting = b2bua_core::caller_tags::fronting(record, "b-3", &callee_tag("b-3"));
    let turn = call.turns().last().expect("the 2xx's turn");
    let to_her = turn.responses_to("a").into_iter().find(|r| r.status() >= 300);
    let to_her = to_her.expect("the non-2xx she is owed");
    assert_eq!(to_her.to().tag(), Some(fronting.as_str()), "under the fronting tag");
    assert!(
        call::helpers::caller_dialog(call.call(), &fronting).is_some(),
        "the held dialog is the fronting one, BYEd on her ACK or at 64·T1"
    );
}

/// The callee's 2xx to a re-INVITE another owner sent, on the early dialog
/// the record holds: the re-INVITE proves the dialog confirmed and the
/// dialled INVITE answered. The 2xx is ACKed, the dialog BYEd past its
/// number, and no CANCEL follows the answer.
#[test]
fn a_2xx_above_the_dialled_number_on_an_early_dialog_confirms_it_and_byes_it() {
    let driver = Driver::new();
    let answered = [Step::Answers("b-1", 200), Step::AliceAcks, Step::Sends("a", Method::Invite)];
    let mut call = losing(ringing(&driver), answered);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Invite, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    let to_bob = turn.methods_to("b-1");
    assert_eq!(to_bob.first(), Some(&("ACK".to_string(), 2)), "{to_bob:?}");
    assert!(to_bob.iter().any(|(m, c)| m == "BYE" && *c == 3), "{to_bob:?}");
    assert!(to_bob.iter().all(|(m, _)| m != "CANCEL"), "{to_bob:?}");
}

/// The same 2xx on a call already ending, its CANCEL of the dialled INVITE
/// out: the CANCEL crossed the answer (RFC 3261 §9.1), so the dialog is
/// confirmed, ACKed and BYEd, never left ending by a CANCEL its INVITE
/// outran.
#[test]
fn a_2xx_above_the_dialled_number_on_a_dialog_ending_by_cancel_confirms_it_and_byes_it() {
    let driver = Driver::new();
    let answered = [Step::Answers("b-1", 200), Step::AliceAcks, Step::Sends("a", Method::Invite)];
    let mut call = losing(ringing(&driver), answered);
    call.run([Step::AliceCancels]);
    assert_eq!(call.call().state, CallModelState::Terminating);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Invite, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    let to_bob = turn.methods_to("b-1");
    assert_eq!(to_bob.first(), Some(&("ACK".to_string(), 2)), "{to_bob:?}");
    assert!(to_bob.iter().any(|(m, c)| m == "BYE" && *c == 3), "{to_bob:?}");
    let leg = call::helpers::find_leg(call.call(), "b-1").expect("the callee's leg");
    let cancelling = DialogState::Ending(call::EndingBy::Cancel);
    assert!(leg.dialogs.iter().all(|d| d.state != cancelling), "no dialog ends by CANCEL");
}

/// The transfer target's 2xx at a CSeq above the INVITE that dialled it
/// answers a later request of its early-held dialog — a re-INVITE another
/// owner sent after the target's answer: the transfer machine does not read
/// it as the target answering its INVITE, and the evidence rule claims it.
#[test]
fn a_transfer_targets_2xx_above_its_dialled_number_is_evidence_not_its_answer() {
    let driver = Driver::new();
    let mut call = bridged(&driver);
    call.run([Step::Sends("b-1", Method::Refer), allowed(), Step::Answers("b-2", 180)]);
    let answered = [Step::Answers("b-2", 200), Step::Sends("a", Method::Invite)];
    let mut call = losing(call, answered);
    let reinvite = call.peer("b-2").and_then(|p| p.last_of(&Method::Invite).cloned());
    assert!(reinvite.is_some_and(|r| r.cseq().seq() > 1), "a re-INVITE reached the target");
    let turn = call.turned(Step::AnswersLast("b-2", Method::Invite, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
}

/// A service rule claiming every response to a BYE or an INFO on the
/// callee's leg.
fn greedy() -> RuleDefinition {
    RuleDefinition::core(
        "greedy-service",
        SERVICE_LAYER,
        &[],
        Match::response().methods(&["BYE", "INFO"]).direction(Direction::FromB),
        |_| Some(RuleHandleResult::new(vec![])),
    )
}

/// The evidence stage reads the event before any rule: a service rule that
/// claims the callee's BYE and INFO finals does not pre-empt a final only
/// a lost write explains (ADR-0055 §4) — the evidence ends the call. A final
/// the record explains is the service rule's.
#[test]
fn a_service_rule_on_the_same_leg_does_not_pre_empt_evidence() {
    let driver = Driver::new().with_rule(greedy());
    let mut call = losing(bridged(&driver), [Step::Sends("a", Method::Bye)]);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Bye, 200));
    assert_eq!(turn.rule, Some("evidence-final"));
    assert_eq!(call.call().state, CallModelState::Terminating, "evidence ends the call");

    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Info)]);
    let turn = call.turned(Step::AnswersSent("b-1", 200));
    assert_eq!(turn.rule, Some("greedy-service"), "the service rule claims a recorded final");
}

/// A rule confirming bob's dialog on his 2xx and ACKing it on this stack's
/// own account, with no body given.
fn acks_bare() -> RuleDefinition {
    RuleDefinition::core(
        "test-ack-bare",
        SERVICE_LAYER,
        &[],
        Match::response().method("INVITE").status_class(2).direction(Direction::FromB),
        |_| {
            Some(RuleHandleResult::new(vec![
                RuleAction::ConfirmDialog { leg_id: "b-1".into() },
                RuleAction::AckLeg { leg_id: "b-1".into(), body: None },
            ]))
        },
    )
}

/// A 2xx carrying an offer — the INVITE that dialled the leg made none —
/// ACKed on this stack's own account with no body given: the ACK still
/// answers the offer, rejecting every stream (RFC 3261 §13.2.2.4, RFC 3264
/// §6), never bare.
#[test]
fn an_authored_bare_ack_of_an_offering_2xx_answers_it_rejecting_every_stream() {
    let driver = Driver::new().with_offerless_caller().with_rule(acks_bare());
    let mut call = ringing(&driver);
    let turn = call.turned(Step::Answers("b-1", 200));
    assert_eq!(turn.rule, Some("test-ack-bare"));
    let ack = turn.request_to("b-1", "ACK").expect("the ACK");
    let answer = String::from_utf8_lossy(ack.body()).to_string();
    assert!(answer.contains("m=audio 0 "), "every stream rejected: {answer}");
}

/// The caller ACKs the 2xx another owner relayed her for her re-INVITE: the
/// ACK is the one bob's 2xx is owed (RFC 3261 §13.2.2.4). It goes to bob at
/// the relayed INVITE's number, and a repeat of bob's 2xx draws it again;
/// the call then ends on the evidence.
#[test]
fn an_ack_of_a_relayed_reinvite_keeps_the_callees_2xx_acked() {
    let driver = Driver::new();
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Invite)]);
    let relayed = call.peer("b-1").and_then(|p| p.last_heard().cloned()).expect("bob's re-INVITE");
    let mut call = losing(call, [Step::AnswersSent("b-1", 200)]);
    let turn = call.turned(Step::AliceAcks);
    assert_eq!(turn.rule, Some("evidence-ack"));
    let ack = turn.request_to("b-1", "ACK").expect("bob's 2xx is ACKed");
    assert_eq!(ack.cseq().seq(), relayed.cseq().seq(), "at the relayed INVITE's number");

    let again = call.turned(Step::AnswersLast("b-1", Method::Invite, 200));
    let reack = again.request_to("b-1", "ACK").expect("the repeat of bob's 2xx is ACKed again");
    assert_eq!(reack.cseq().seq(), relayed.cseq().seq());
}
