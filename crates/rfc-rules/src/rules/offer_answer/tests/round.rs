//! The §13.2 round: answered once, in the right message, never overtaken.

use super::*;

// ---- ack-body-after-complete-offer-answer --------------------------

#[test]
fn ack_with_no_body_after_a_complete_round_is_clean() {
    let msgs = t38_call(None, None);
    let out = run(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(out.len(), 2, "one occasion per ACK: {out:?}");
    assert!(out.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{out:?}");
}

#[test]
fn ack_carrying_a_body_on_an_answered_round_is_flagged() {
    let msgs = t38_call(None, Some(STRAY_ANSWER));
    let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, ALICE);
    assert_eq!(f[0].taker, BOB);
    assert_eq!(f[0].cseq, 3);
    let Decision::Violated(Evidence::AckBodyOnClosedRound {
        offer_msg, answer_msg, streams, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*offer_msg, 4, "the T.38 re-INVITE carried the offer");
    assert_eq!(*answer_msg, 5, "its 200 carried the answer");
    assert_eq!(streams, &vec!["audio/RTP/AVP".to_string()]);
}

#[test]
fn ack_body_on_the_establishing_round_answered_by_a_180_is_flagged() {
    let msgs = t38_call(Some(STRAY_ANSWER), None);
    let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AckBodyOnClosedRound { answer_msg, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*answer_msg, 1, "the 180 closed the round, not the bodyless 200");
}

#[test]
fn delayed_offer_ack_carrying_the_answer_is_clean() {
    // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
    ];
    assert!(violations(&AckBodyAfterCompleteOfferAnswer, &msgs).is_empty());
}

#[test]
fn an_ack_from_a_body_less_vantage_is_undecidable() {
    let mut msgs = t38_call(None, Some(STRAY_ANSWER));
    for m in &mut msgs {
        m.body = None;
    }
    let out = run(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|f| !f.decided()), "{out:?}");
}

#[test]
fn a_retransmitted_offending_ack_is_one_occasion() {
    let mut msgs = t38_call(None, Some(STRAY_ANSWER));
    let mut again = msgs[6].clone();
    again.at_us = 8;
    again.repeat = true;
    msgs.push(again);
    let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(f.len(), 1, "a retransmission is not a second ACK: {f:?}");
}

// ---- final-2xx-answers-the-offer ------------------------------------

/// A reliable provisional: `Require: 100rel` and an `RSeq`, so its
/// description BINDS (RFC 3262 §5).
fn reliable_1xx(at_us: u64, status: u16, cseq: u32, body: Option<&str>) -> Msg {
    Msg {
        head: Some(head("Require: 100rel\r\nRSeq: 1\r\n", body)),
        ..resp(at_us, BOB, ALICE, status, cseq, "INVITE", body)
    }
}

#[test]
fn a_2xx_carrying_the_answer_meets_the_offer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1, "one occasion, the 2xx: {out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
}

/// RFC 5621 §3.1: the answer rides inside a `multipart/mixed` body beside
/// another part, and the reading takes the SDP part as the description.
#[test]
fn an_answer_framed_in_a_multipart_body_meets_the_offer() {
    use sip_message::{compose_multipart, MultipartPart};
    let framed = compose_multipart(
        "multipart/mixed",
        &[
            MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15]),
            MultipartPart::new("application/sdp", AUDIO_ANSWER.as_bytes().to_vec()),
        ],
    )
    .unwrap();
    let answer = Msg {
        head: Some(format!("X: y\r\nContent-Type: {}\r\n\r\n", framed.content_type).into_bytes()),
        body: Some(framed.body),
        ..resp(2, BOB, ALICE, 200, 1, "INVITE", None)
    };
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        answer,
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1, "one occasion, the 2xx: {out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
    let parse = run(&SdpBodyParseable, &msgs);
    assert!(parse.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{parse:?}");
}

#[test]
fn a_bodiless_2xx_leaves_the_offer_unanswered() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the answerer is charged");
    assert_eq!(f[0].taker, ALICE);
    assert_eq!(f[0].cseq, 1);
    let Decision::Violated(Evidence::OfferLeftUnanswered {
        unanswered_final_msg,
        status,
        offer_msg,
        offered_streams,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*unanswered_final_msg, 1);
    assert_eq!(*status, 200);
    assert_eq!(*offer_msg, 0);
    assert_eq!(offered_streams, &vec!["audio/RTP/AVP".to_string()]);
}

#[test]
fn a_reliable_provisional_answer_discharges_the_2xx() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        reliable_1xx(2, 183, 1, Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", None),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1, "the reliable 183 is no occasion of its own: {out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
}

#[test]
fn early_media_in_an_unreliable_provisional_answers_nothing() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 183, 1, "INVITE", Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", None),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "a plan the peer may re-latch on is no answer: {f:?}");
}

#[test]
fn a_failure_final_owes_no_answer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 486, 1, "INVITE", None),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
}

#[test]
fn a_cancelled_round_is_no_occasion() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 180, 1, "INVITE", None),
        req(3, ALICE, BOB, "CANCEL", 1, None, None),
        resp(4, BOB, ALICE, 200, 1, "CANCEL", None),
        resp(5, BOB, ALICE, 487, 1, "INVITE", None),
        req(6, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
}

#[test]
fn a_round_the_recording_cuts_off_is_no_occasion() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 180, 1, "INVITE", None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "no answer YET is no absence");
}

#[test]
fn a_delayed_offer_answered_in_the_ack_is_no_occasion() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "the ACK answers, not the 2xx");
}

/// §13.2.2.4: the ACK MUST answer the offer a delayed-offer 2xx carried;
/// a bodyless ACK leaves it unanswered, charged to the ACK's sender.
#[test]
fn a_delayed_offer_acked_without_an_answer_is_violated() {
    let answered = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
    ];
    assert!(violations(&DelayedOfferAnsweredInAck, &answered).is_empty());
    let silent = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&DelayedOfferAnsweredInAck, &silent);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, ALICE, "the ACK's sender owed the answer");
    assert_eq!(f[0].anchor, 2, "anchored at the ACK");
    // An offer a reliable 183 carried closed by PRACK (RFC 3262 §5): the
    // 2xx's description offers nothing, and the ACK owes no answer.
    let reliable = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        reliable_1xx(2, 183, 1, Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "PRACK", 2, Some("bt"), Some(AUDIO_ANSWER)),
        resp(4, BOB, ALICE, 200, 2, "PRACK", None),
        resp(5, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(6, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(
        run(&DelayedOfferAnsweredInAck, &reliable).is_empty(),
        "{:?}",
        run(&DelayedOfferAnsweredInAck, &reliable)
    );
    // An offer in the INVITE leaves the ACK owing nothing.
    let early = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(run(&DelayedOfferAnsweredInAck, &early).is_empty(), "no occasion");
}

#[test]
fn a_bodiless_2xx_to_a_bodiless_invite_is_no_occasion() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
}

#[test]
fn a_re_invite_owes_its_own_answer() {
    let msgs = t38_call(None, None);
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "the establishing round's bodiless 200: {f:?}");
    assert_eq!(f[0].cseq, 1);
    let mut cut = t38_call(None, None);
    cut[5] = resp(6, BOB, ALICE, 200, 3, "INVITE", None);
    let f = violations(&Final2xxAnswersTheOffer, &cut);
    assert_eq!(f.len(), 2, "the T.38 re-offer owes an answer too: {f:?}");
    assert_eq!(f[1].cseq, 3);
}

#[test]
fn an_update_owes_its_own_answer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "UPDATE", 2, Some("bt"), Some(T38_OFFER)),
        resp(5, BOB, ALICE, 200, 2, "UPDATE", None),
    ];
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].cseq, 2);
}

#[test]
fn a_prack_answering_a_reliable_1xx_offer_is_no_occasion() {
    // RFC 3262 §5's delayed-offer form: the offer rides the reliable 183,
    // the PRACK carries the answer, and the 200 to the PRACK owes nothing.
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        reliable_1xx(2, 183, 1, Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "PRACK", 2, Some("bt"), Some(AUDIO_ANSWER)),
        resp(4, BOB, ALICE, 200, 2, "PRACK", None),
        resp(5, BOB, ALICE, 200, 1, "INVITE", None),
        req(6, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
}

#[test]
fn two_early_dialogs_sharing_a_cseq_are_two_rounds() {
    // Each early dialog numbers its own requests (§12.2.1.1), so fork 1's
    // bodiless UPDATE and fork 2's re-offer both ride CSeq 3. Only fork 2
    // opened a round, and fork 1's 200 answers nothing it took.
    let tagged = |m: Msg, tag: &str| Msg { to_tag: Some(tag.to_string()), ..m };
    let msgs = vec![
        tagged(req(1, ALICE, BOB, "UPDATE", 3, Some("f2"), Some(T38_OFFER)), "f2"),
        tagged(resp(2, BOB, ALICE, 200, 3, "UPDATE", Some(T38_ANSWER)), "f2"),
        tagged(req(3, ALICE, BOB, "UPDATE", 3, Some("f1"), None), "f1"),
        tagged(resp(4, BOB, ALICE, 200, 3, "UPDATE", None), "f1"),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1, "only fork 2 carried an offer: {out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
}

#[test]
fn an_options_capability_body_is_no_offer() {
    let msgs = vec![
        req(1, ALICE, BOB, "OPTIONS", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "OPTIONS", None),
    ];
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "§11.2 is not a round");
}

#[test]
fn a_final_from_a_body_less_vantage_settles_nothing() {
    let mut msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
    ];
    for m in &mut msgs {
        m.body = None;
    }
    // A body-less vantage carries no offer either, so the round never opens.
    assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
    let mut half = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
    ];
    half[1].body = None;
    let out = run(&Final2xxAnswersTheOffer, &half);
    assert_eq!(out.len(), 1);
    assert!(!out[0].decided(), "{out:?}");
}

#[test]
fn a_final_carrying_bytes_that_are_no_description_settles_nothing() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some("--b\r\nContent-Type: x\r\n")),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1);
    assert!(!out[0].decided(), "a multipart the reading cannot open is no proof: {out:?}");
}

#[test]
fn a_retransmitted_bodiless_final_is_one_occasion() {
    let mut msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
    ];
    let mut again = msgs[1].clone();
    again.at_us = 3;
    again.repeat = true;
    msgs.push(again);
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "a retransmission is not a second final: {f:?}");
}

#[test]
fn two_forks_each_owe_their_own_dialog_an_answer() {
    let mut answered = resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER));
    answered.to_tag = Some("fork-a".to_string());
    let mut silent = resp(3, BOB, ALICE, 200, 1, "INVITE", None);
    silent.to_tag = Some("fork-b".to_string());
    let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)), answered, silent];
    let f = violations(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(f.len(), 1, "one fork answered, the other did not: {f:?}");
    assert_eq!(f[0].anchor, 2);
}

// ---- second-answer-repeats-the-first --------------------------------

/// The same media set as [`AUDIO_ANSWER`] at another address and port.
const MOVED_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.3\r\n\
t=0 0\r\n\
m=audio 41000 RTP/AVP 8\r\n";

/// A media set [`AUDIO_ANSWER`]'s is not.
const WIDER_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n\
m=video 60790 RTP/AVP 96\r\n";

/// A reliable provisional carrying `Require: 100rel` and an `RSeq` — its
/// description binds (RFC 3262 §5).
fn reliable(at_us: u64, status: u16, body: Option<&str>) -> Msg {
    Msg {
        head: Some(head("Require: 100rel\r\nRSeq: 1\r\n", body)),
        ..resp(at_us, BOB, ALICE, status, 1, "INVITE", body)
    }
}

/// §13.2.1's everyday form: the reliable provisional states the answer and
/// the 2xx repeats it, which is what the RFC admits.
#[test]
fn a_second_answer_repeating_the_plan_is_clean() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        reliable(2, 183, Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
    assert_eq!(out.len(), 1, "the 2xx is the one occasion: {out:?}");
    assert_eq!((out[0].emitter.as_str(), out[0].cseq), (BOB, 1), "the answerer");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

/// Early media in an UNRELIABLE provisional states no answer: an
/// announcement the peer may re-latch on before the callee answers is
/// RFC 3960's shape, and the 2xx is the dialog's first binding answer.
#[test]
fn early_media_in_an_unreliable_provisional_binds_nothing() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 183, 1, "INVITE", Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", Some(MOVED_ANSWER)),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
    assert!(out.is_empty(), "the 2xx is the answer, not a second one: {out:?}");
}

#[test]
fn a_second_answer_moving_the_media_is_flagged() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        reliable(2, 183, Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", Some(MOVED_ANSWER)),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&SecondAnswerRepeatsTheFirst, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (2, BOB), "the 2xx, on its sender");
    let Decision::Violated(Evidence::SecondAnswerDiverged {
        offer_msg,
        first_answer_msg,
        first_plan,
        second_plan,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((*offer_msg, *first_answer_msg), (0, 1));
    assert_eq!(
        first_plan,
        &vec!["c=IN IP4 10.0.0.2".to_string(), "audio 60788 RTP/AVP 8".to_string()]
    );
    assert_eq!(
        second_plan,
        &vec!["c=IN IP4 10.0.0.3".to_string(), "audio 41000 RTP/AVP 8".to_string()]
    );
}

/// The control the corpus supplies: two forks answering one INVITE carry
/// two genuinely different answers under two To tags, and neither is a
/// second answer to the other.
#[test]
fn two_forks_answer_one_offer_and_neither_is_a_second_answer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp_fork(2, 200, 1, "b1", Some(AUDIO_ANSWER)),
        resp_fork(3, 200, 1, "b2", Some(WIDER_ANSWER)),
    ];
    let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
    assert!(out.is_empty(), "each fork closes its own dialog: {out:?}");
}

/// The same two answers folded onto ONE To tag — the collapsed fork the
/// obligation exists for.
#[test]
fn a_collapsed_fork_answering_twice_on_one_dialog_is_flagged() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp_fork(2, 200, 1, "b1", Some(AUDIO_ANSWER)),
        resp_fork(3, 200, 1, "b1", Some(WIDER_ANSWER)),
    ];
    let f = violations(&SecondAnswerRepeatsTheFirst, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 2, "the second 2xx on the one dialog");
    let Decision::Violated(Evidence::SecondAnswerDiverged { second_plan, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(second_plan.len(), 3, "c= plus two streams: {second_plan:?}");
}

#[test]
fn a_retransmitted_answer_is_not_a_second_answer() {
    let mut msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
    ];
    let mut again = msgs[1].clone();
    again.at_us = 3;
    again.repeat = true;
    msgs.push(again);
    let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
    assert!(out.is_empty(), "a retransmission states no second answer: {out:?}");
}

/// A re-INVITE opens a round of its own, so its answer answers THAT offer
/// and is no second answer to the first round's.
#[test]
fn a_re_invite_answer_is_no_second_answer() {
    let out = run(&SecondAnswerRepeatsTheFirst, &t38_call(None, None));
    assert!(out.is_empty(), "{out:?}");
}

// ---- no-new-offer-while-offer-pending ------------------------------

#[test]
fn a_serialised_offer_round_is_clean() {
    let out = run(&NoNewOfferWhileOfferPending, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "one occasion per offer sent: {out:?}");
    assert_eq!(out[0].emitter, ALICE, "the offerer is charged");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn a_second_offer_over_an_unanswered_one_is_flagged() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
        req(2, ALICE, BOB, "INVITE", 2, None, Some(OFFER_1AUDIO)),
    ];
    let f = violations(&NoNewOfferWhileOfferPending, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (1, ALICE));
    let Decision::Violated(Evidence::OfferWhilePending {
        pending_offer_msg,
        pending_offer_cseq,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((*pending_offer_msg, *pending_offer_cseq), (0, 1));
}

/// §5 charges an agent for overtaking ITS OWN offer; a round it merely took
/// is the peer's to answer and is the peer's occasion.
#[test]
fn an_offer_the_agent_only_took_never_blocks_its_own() {
    let msgs = vec![
        req(1, BOB, ALICE, "INVITE", 7, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
    ];
    assert!(violations(&NoNewOfferWhileOfferPending, &msgs).is_empty());
}

#[test]
fn a_delayed_offer_in_the_2xx_is_its_senders_own_offer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
    ];
    let out = run(&NoNewOfferWhileOfferPending, &msgs);
    assert_eq!(out.len(), 1, "the 2xx offer is the one occasion: {out:?}");
    assert_eq!(out[0].emitter, BOB);
    assert!(matches!(out[0].decision, Decision::Compliant));
}

// ---- a description present, unreadable ------------------------------

/// A body declared `application/sdp` that opens with no `v=` line (RFC 4566
/// §5): a description the sender stated, which the reading cannot parse.
const UNREADABLE_OFFER: &str = "o=alice 424242 1 IN IP4 10.0.0.1\n\
s=-\n\
c=IN IP4 10.0.0.1\n\
t=0 0\n\
m=audio 27500 RTP/AVP 8\n";

/// The INVITE carried a description, unreadable: the 2xx describing a plan is
/// its answer, never a delayed offer, so the bodyless ACK owes nothing.
#[test]
fn an_unreadable_offer_makes_no_delayed_offer_of_the_answer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(UNREADABLE_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(
        violations(&DelayedOfferAnsweredInAck, &msgs).is_empty(),
        "{:?}",
        run(&DelayedOfferAnsweredInAck, &msgs)
    );
    assert!(violations(&AckBodyAfterCompleteOfferAnswer, &msgs).is_empty());
}

/// The answerer of an unreadable offer holds no offer of its own pending, so
/// its later offer in a 2xx to a bodyless re-INVITE overtakes nothing.
#[test]
fn an_answer_to_an_unreadable_offer_leaves_no_offer_of_its_own_pending() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(UNREADABLE_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 2, Some("bt"), None),
        resp(5, BOB, ALICE, 200, 2, "INVITE", Some(T38_ANSWER)),
        req(6, ALICE, BOB, "ACK", 2, Some("bt"), Some(T38_OFFER)),
    ];
    assert!(
        violations(&NoNewOfferWhileOfferPending, &msgs).is_empty(),
        "{:?}",
        run(&NoNewOfferWhileOfferPending, &msgs)
    );
}

/// A bodyless 2xx over an unreadable offer: the round is the offerer's, and
/// what it offered cannot be read, so the answer it owed is undecided.
#[test]
fn a_bodiless_final_over_an_unreadable_offer_is_undecidable() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(UNREADABLE_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&Final2xxAnswersTheOffer, &msgs);
    assert_eq!(out.len(), 1, "the 2xx is the occasion: {out:?}");
    assert!(matches!(out[0].decision, Decision::Undecidable(_)), "{:?}", out[0].decision);
}

/// An ACK body on a round whose offer the reading cannot parse settles nothing.
#[test]
fn an_ack_body_on_a_round_with_an_unreadable_offer_is_undecidable() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(UNREADABLE_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(STRAY_ANSWER)),
    ];
    let out = run(&AckBodyAfterCompleteOfferAnswer, &msgs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Undecidable(_)), "{:?}", out[0].decision);
}

/// A delayed offer the reading cannot parse leaves the ACK it owed undecided.
#[test]
fn a_bodiless_ack_to_an_unreadable_delayed_offer_is_undecidable() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, None),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(UNREADABLE_OFFER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&DelayedOfferAnsweredInAck, &msgs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Undecidable(_)), "{:?}", out[0].decision);
}

/// An offer sent over the agent's own pending offer that the reading cannot
/// parse is undecided: what was pending cannot be read.
#[test]
fn an_offer_over_an_unreadable_pending_offer_is_undecidable() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(UNREADABLE_OFFER)),
        req(2, ALICE, BOB, "INVITE", 2, None, Some(OFFER_1AUDIO)),
    ];
    let out = run(&NoNewOfferWhileOfferPending, &msgs);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(matches!(out[1].decision, Decision::Undecidable(_)), "{:?}", out[1].decision);
}

/// An unreliable provisional binds no answer (RFC 3262 §5): one the reading
/// cannot parse answers nothing, and the 2xx's answer is still the one read.
#[test]
fn an_unreadable_unreliable_provisional_leaves_the_final_the_answer() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
        resp(2, BOB, ALICE, 183, 1, "INVITE", Some(UNREADABLE_OFFER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "the 2xx re-typed the offered stream: {f:?}");
    assert_eq!(f[0].anchor, 2, "judged at the 2xx");
}
