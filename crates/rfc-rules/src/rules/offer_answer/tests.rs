use super::*;
use crate::wire::Observation;

const ALICE: &str = "10.0.0.1:5060";
const BOB: &str = "10.0.0.2:5070";

const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

/// The harness default body that must never ride an ACK: a fresh session
/// (`o=bob 1 1`) offering audio over RTP/AVP.
const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

/// The head a synthetic message carries: `extra` lines, and the
/// `Content-Type` a body demands (RFC 3261 §20.15) — `application/sdp`,
/// which is what every description these tests carry declares.
fn head(extra: &str, body: Option<&str>) -> Vec<u8> {
    let declared = match body {
        Some(b) if !b.is_empty() => "Content-Type: application/sdp\r\n",
        _ => "",
    };
    format!("X: y\r\n{extra}{declared}\r\n").into_bytes()
}

fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    cseq: u32,
    to_tag: Option<&str>,
    body: Option<&str>,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "cid-1@host".to_string(),
        cseq,
        cseq_method: method.to_string(),
        via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
        from_tag: Some("at".to_string()),
        to_tag: to_tag.map(str::to_string),
        head: Some(head("", body)),
        body: Some(body.unwrap_or("").as_bytes().to_vec()),
    }
}

fn resp(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    method: &str,
    body: Option<&str>,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "cid-1@host".to_string(),
        cseq,
        cseq_method: method.to_string(),
        via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        head: Some(head("", body)),
        body: Some(body.unwrap_or("").as_bytes().to_vec()),
    }
}

fn view(msgs: &[Msg]) -> (Observation, &[Msg]) {
    (Observation { closed: true, ..Observation::default() }, msgs)
}

fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    let (obs, msgs) = view(msgs);
    rule.eval(&WireView { msgs, obs: &obs })
}

fn violations(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    run(rule, msgs).into_iter().filter(Finding::violated).collect()
}

/// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
/// 200(answer) → ACK. The ACK bodies are the caller's choice.
fn t38_call(first_ack: Option<&str>, second_ack: Option<&str>) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 180, 1, "INVITE", Some(AUDIO_ANSWER)),
        resp(3, BOB, ALICE, 200, 1, "INVITE", None),
        req(4, ALICE, BOB, "ACK", 1, Some("bt"), first_ack),
        req(5, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
        resp(6, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)),
        req(7, ALICE, BOB, "ACK", 3, Some("bt"), second_ack),
    ]
}

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

/// The same media set as [`AUDIO_ANSWER`] at another address and port —
/// the shape 11 of the corpus's 59 two-dialog reroutes differ by.
const MOVED_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.3\r\n\
t=0 0\r\n\
m=audio 41000 RTP/AVP 8\r\n";

/// A media set [`AUDIO_ANSWER`]'s is not — the shape the other 48 differ
/// by.
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

// ---- answer-stream-matches-offer -----------------------------------

#[test]
fn answer_keeping_media_type_and_proto_is_clean() {
    // Judged from the ANSWERER: it takes the offer and sends the answer.
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(T38_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let out = run(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
    assert_eq!(out[0].emitter, BOB, "the answerer is charged");
}

#[test]
fn answer_re_typing_the_stream_is_flagged() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped {
        stream_indexes, offered, answered, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0]);
    assert_eq!(offered, &vec!["image 27500 udptl".to_string()]);
    assert_eq!(answered, &vec!["audio 60788 RTP/AVP".to_string()]);
}

#[test]
fn answer_re_transporting_the_stream_is_flagged() {
    const SAVP_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/SAVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(SAVP_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped { offered, .. }) = &f[0].decision else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(offered, &vec!["audio 27500 RTP/AVP".to_string()]);
}

#[test]
fn rejected_stream_keeps_type_and_proto_at_port_zero() {
    const REJECTED: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(REJECTED)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    assert!(violations(&AnswerStreamMatchesOffer, &msgs).is_empty());
}

#[test]
fn every_re_typed_stream_of_one_answer_rides_one_finding() {
    const TWO_STREAM_OFFER: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
m=video 27502 RTP/AVP 96\r\n";
    const TWO_STREAM_ANSWER: &str = "v=0\r\n\
o=bob 2 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n\
m=audio 60790 RTP/SAVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(TWO_STREAM_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(TWO_STREAM_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
    let Decision::Violated(Evidence::AnswerStreamRetyped { stream_indexes, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0, 1], "both streams named on the one finding");
}

#[test]
fn a_second_round_is_judged_like_the_first() {
    const RETYPED_T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
        resp(5, BOB, ALICE, 200, 3, "INVITE", Some(RETYPED_T38_ANSWER)),
        req(6, ALICE, BOB, "ACK", 3, Some("bt"), None),
    ];
    let f = violations(&AnswerStreamMatchesOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].cseq, 3, "the second round's offer transaction");
}

// ---- sdp-origin-continuity -----------------------------------------

#[test]
fn rising_session_version_on_one_origin_is_clean() {
    let msgs = t38_call(None, None);
    assert!(violations(&SdpOriginContinuity, &msgs).is_empty());
}

#[test]
fn a_foreign_origin_mid_dialog_is_flagged() {
    let msgs = t38_call(None, Some(STRAY_ANSWER));
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 6, "the ACK carrying the foreign origin");
    let Decision::Violated(Evidence::SdpOriginDiverged {
        same_session,
        origin_line,
        prior_origin_line,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert!(!same_session);
    assert_eq!(origin_line, "o=bob 1 1 IN IP4 127.0.0.1");
    assert_eq!(prior_origin_line, "o=alice 424242 2 IN IP4 10.0.0.1");
}

#[test]
fn a_lowered_session_version_is_flagged() {
    const STALE_REOFFER: &str = "v=0\r\n\
o=alice 424242 0 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(STALE_REOFFER)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::SdpOriginDiverged {
        same_session,
        session_version,
        prior_session_version,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert!(same_session, "same session, stale revision");
    assert_eq!((*prior_session_version, *session_version), (1, 0));
}

/// The address is part of the identity, so a re-offer from a new one is a
/// different session even where username and sess-id repeat.
#[test]
fn a_moved_origin_address_is_a_different_session() {
    const MOVED: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 192.0.2.9\r\n\
s=-\r\n\
c=IN IP4 192.0.2.9\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(MOVED)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::SdpOriginDiverged { same_session, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert!(!same_session);
}

/// RFC 3264 §8: "If the version in the origin line does not increment, the
/// SDP MUST be identical to the SDP with that version number." A changed
/// description owes exactly +1; an identical one may keep its version or
/// take +1, and nothing more.
#[test]
fn the_version_tracks_what_the_description_says() {
    const BUMPED_TWICE: &str = "v=0\r\n\
o=alice 424242 3 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(BUMPED_TWICE)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "a changed body owes exactly +1: {f:?}");

    const CHANGED_UNBUMPED: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(CHANGED_UNBUMPED)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "a changed body under an unchanged version: {f:?}");
    let Decision::Violated(Evidence::SdpOriginDiverged { body_changed, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert!(body_changed);

    const BUMPED_UNCHANGED: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(BUMPED_UNCHANGED)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert!(f.is_empty(), "an identical description may take +1: {f:?}");

    const REPEATED: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(REPEATED)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert!(f.is_empty(), "an identical description may keep its version: {f:?}");

    const IDENTICAL_BUMPED_TWICE: &str = "v=0\r\n\
o=alice 424242 3 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(IDENTICAL_BUMPED_TWICE)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "an identical description takes at most +1: {f:?}");
}

#[test]
fn a_taken_peer_origin_never_lands_on_the_taker() {
    // ALICE only TAKES bob's descriptions here — its own origin stream is
    // empty, so bob's discontinuity is bob's finding, on bob's slot.
    let msgs = vec![
        req(1, BOB, ALICE, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, ALICE, BOB, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, BOB, ALICE, "ACK", 1, Some("bt"), Some(STRAY_ANSWER)),
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert!(f.iter().all(|f| f.emitter == BOB), "{f:?}");
}

#[test]
fn a_description_with_no_readable_origin_settles_nothing() {
    const NO_ORIGIN: &str = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 27500 RTP/AVP 8\r\n";
    // Same origin as AUDIO_OFFER, changed description, version not moved.
    const STALE_AFTER_THE_GAP: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(NO_ORIGIN)),
        // The stream's predecessor is the last description that HAD an
        // origin, so the stale re-offer is still caught across the gap.
        req(3, ALICE, BOB, "INVITE", 5, Some("bt"), Some(STALE_AFTER_THE_GAP)),
    ];
    let out = run(&SdpOriginContinuity, &msgs);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out.iter().any(|f| f.anchor == 1 && !f.decided()));
    let hit = out.iter().find(|f| f.anchor == 2).expect("the re-stated origin");
    let Decision::Violated(Evidence::SdpOriginDiverged { prior_origin_msg, .. }) = &hit.decision
    else {
        panic!("{:?}", hit.decision);
    };
    assert_eq!(*prior_origin_msg, 0, "compared against the last readable origin");
}

// ---- the RFC 3264 offer/answer-model family -------------------------

/// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

/// Its answer: same stream table, same `t=`, same payload-type bindings.
const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

const OFFER_2MEDIA: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

/// A response presenting a chosen early dialog — the To tag that tells two
/// forks of one INVITE apart.
fn resp_fork(at_us: u64, status: u16, cseq: u32, to_tag: &str, body: Option<&str>) -> Msg {
    Msg {
        to_tag: Some(to_tag.to_string()),
        ..resp(at_us, BOB, ALICE, status, cseq, "INVITE", body)
    }
}

/// INVITE(offer) → 200(answer) → ACK: one round, alice offering.
fn one_round(offer: &str, answer: &str) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(offer)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(answer)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
    ]
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

// ---- answer-m-line-count-matches-offer ------------------------------

#[test]
fn an_answer_with_the_offers_stream_count_is_clean() {
    let out = run(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].emitter, BOB, "the answerer is charged");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_adding_a_stream_is_flagged() {
    let f = violations(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_2MEDIA));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerMLineCountDiffers {
        offer_m_lines,
        answer_m_lines,
        answered_streams,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((*offer_m_lines, *answer_m_lines), (1, 2));
    assert_eq!(answered_streams, &vec!["audio/RTP/AVP".to_string(), "video/RTP/AVP".to_string()]);
}

/// Two forks are two answers to ONE offer (the To tag partitions the
/// round's closures), so each fork's answer is judged on its own dialog and
/// a bad one behind a clean fork is not hidden by it.
#[test]
fn each_fork_answers_the_one_offer_on_its_own_dialog() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
        resp_fork(2, 180, 1, "b1", None),
        resp_fork(3, 200, 1, "b2", Some(ANSWER_2MEDIA)),
        resp_fork(4, 200, 1, "b1", Some(ANSWER_1AUDIO)),
    ];
    let out = run(&AnswerMLineCountMatchesOffer, &msgs);
    assert_eq!(out.len(), 2, "one occasion per fork's answer: {out:?}");
    let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
    assert_eq!(f.len(), 1, "only the fork that answered wrongly: {f:?}");
    assert_eq!(f[0].anchor, 2, "the two-stream answer on fork b2");
}

// ---- answer-t-line-equals-offer -------------------------------------

#[test]
fn an_answer_repeating_the_t_line_is_clean() {
    let out = run(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_rewriting_the_t_line_is_flagged() {
    let answer = ANSWER_1AUDIO.replace("t=0 0", "t=100 200");
    let f = violations(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::AnswerTLineDiffers { offer_t_line, answer_t_line, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((offer_t_line.as_str(), answer_t_line.as_str()), ("0 0", "100 200"));
}

// ---- answer-media-type-matches-offer --------------------------------

#[test]
fn an_answer_keeping_the_media_type_at_each_index_is_clean() {
    let out = run(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, ANSWER_2MEDIA));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn every_index_one_answer_swapped_rides_one_finding() {
    let swapped = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=sendrecv\r\n\
m=audio 50002 RTP/AVP 0\r\n\
a=sendrecv\r\n";
    let f = violations(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, swapped));
    assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
    let Decision::Violated(Evidence::AnswerMediaTypeMismatched {
        stream_indexes,
        offered_types,
        answered_types,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0, 1], "both positions named on the one finding");
    assert_eq!(offered_types, &vec!["audio".to_string(), "video".to_string()]);
    assert_eq!(answered_types, &vec!["video".to_string(), "audio".to_string()]);
}

// ---- direction-pair-valid -------------------------------------------

#[test]
fn a_sendrecv_pair_is_clean() {
    let out = run(&DirectionPairValid, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_of_sendonly_to_sendonly_is_flagged() {
    let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=sendonly");
    let answer = ANSWER_1AUDIO.replace("a=sendrecv", "a=sendonly");
    let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::DirectionPairInvalid {
        offered_directions,
        answered_directions,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(offered_directions, &vec!["sendonly".to_string()]);
    assert_eq!(answered_directions, &vec!["sendonly".to_string()]);
}

/// An `m=` block with no direction attribute states `sendrecv` (§6.1), so
/// an offer of `inactive` answered by a bare block is a real mis-pairing.
#[test]
fn a_bare_answer_block_reads_as_sendrecv() {
    let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
    let answer = ANSWER_1AUDIO.replace("a=sendrecv\r\n", "");
    let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::DirectionPairInvalid { answered_directions, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(answered_directions, &vec!["sendrecv".to_string()]);
}

// ---- rejected-stream-minimal-answer ---------------------------------

#[test]
fn a_rejection_keeping_a_format_token_is_clean() {
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0");
    let out = run(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn a_bare_rejection_is_flagged() {
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP");
    let f = violations(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::RejectedStreamWithoutFormat {
        stream_indexes,
        rejected_rows,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(stream_indexes, &vec![0]);
    assert_eq!(rejected_rows, &vec!["audio 0 RTP/AVP".to_string()]);
}

// ---- re-offer-m-line-count-monotonic --------------------------------

#[test]
fn a_re_offer_disabling_a_stream_in_place_is_clean() {
    let reoffer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
    let out = run(&ReOfferMLineCountMonotonic, &msgs);
    assert_eq!(out.len(), 1, "one occasion per step of alice's stream: {out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn a_re_offer_dropping_a_stream_slot_is_flagged() {
    let mut msgs = one_round(OFFER_2MEDIA, ANSWER_2MEDIA);
    msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(OFFER_1AUDIO)));
    let f = violations(&ReOfferMLineCountMonotonic, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (3, ALICE));
    let Decision::Violated(Evidence::ReOfferStreamsDropped {
        m_lines,
        prior_m_lines,
        prior_offer_msg,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!((*prior_m_lines, *m_lines, *prior_offer_msg), (2, 1, 0));
}

// ---- zero-port-propagation ------------------------------------------

#[test]
fn an_answer_keeping_a_disabled_stream_disabled_is_clean() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0].decision, Decision::Compliant));
}

#[test]
fn an_answer_reviving_a_disabled_stream_is_flagged() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let f = violations(&ZeroPortPropagation, &one_round(&offer, ANSWER_1AUDIO));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ZeroPortResurrected {
        stream_indexes, answered_ports, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(
        (stream_indexes.as_slice(), answered_ports.as_slice()),
        ([0].as_slice(), [50000].as_slice())
    );
}

#[test]
fn an_answer_stating_no_readable_port_settles_nothing() {
    let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio - RTP/AVP 0 96");
    let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(!out[0].decided(), "{out:?}");
}

// ---- payload-type-mapping-stable ------------------------------------

#[test]
fn a_re_offer_repeating_its_bindings_is_clean() {
    let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(OFFER_1AUDIO)));
    let out = run(&PayloadTypeMappingStable, &msgs);
    assert_eq!(out.len(), 3, "one occasion per description sent: {out:?}");
    assert!(out.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{out:?}");
}

#[test]
fn a_re_offer_rebinding_a_payload_type_is_flagged() {
    let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
    let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
    let f = violations(&PayloadTypeMappingStable, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (3, ALICE));
    let Decision::Violated(Evidence::PayloadTypeRemapped {
        payload_types,
        prior_encodings,
        encodings,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(payload_types, &vec!["96".to_string()]);
    assert_eq!(prior_encodings, &vec!["opus/48000/2".to_string()]);
    assert_eq!(encodings, &vec!["H264/90000".to_string()]);
}

/// Encoding names are MIME subtypes: one binding, whatever the case.
#[test]
fn a_re_offer_re_spelling_the_encoding_is_clean() {
    let reoffer = OFFER_1AUDIO.replace("a=rtpmap:0 PCMU/8000", "a=rtpmap:0 pcmu/8000");
    let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
    assert!(violations(&PayloadTypeMappingStable, &msgs).is_empty());
}

/// The bindings are the SENDER's: §3264 lets the two ends number the same
/// codec differently, so the answerer's own numbering rebinds nothing.
#[test]
fn the_peers_own_numbering_is_not_the_agents_remap() {
    let answer = ANSWER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
    assert!(violations(&PayloadTypeMappingStable, &one_round(OFFER_1AUDIO, &answer)).is_empty());
}

/// A binding one `m=` block states binds the blocks below it too.
#[test]
fn a_description_rebinding_a_payload_type_within_itself_is_flagged() {
    let offer = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 96\r\n\
a=rtpmap:96 opus/48000/2\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n";
    let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(offer))];
    let f = violations(&PayloadTypeMappingStable, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::PayloadTypeRemapped { payload_types, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(payload_types, &vec!["96".to_string()]);
}

/// An audio offer binding payload type 8 to `enc`.
fn audio_pt8(enc: &str) -> String {
    format!(
        "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
a=rtpmap:8 {enc}\r\n\
a=sendrecv\r\n"
    )
}

/// Two descriptions alice sent on one call, binding payload type 8 the way
/// each names it.
fn pt8_stream(first: &str, then: &str) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(&audio_pt8(first))),
        req(2, ALICE, BOB, "INVITE", 2, Some("bt"), Some(&audio_pt8(then))),
    ]
}

/// RFC 4566 §6 makes the channel count optional on an audio stream and its
/// default one, so the two spellings state one binding — in either order.
#[test]
fn an_absent_audio_channel_count_is_one_channel() {
    for (first, then) in [("PCMA/8000", "PCMA/8000/1"), ("PCMA/8000/1", "PCMA/8000")] {
        let f = violations(&PayloadTypeMappingStable, &pt8_stream(first, then));
        assert!(f.is_empty(), "{first} then {then}: {f:?}");
    }
}

/// The default settles the channel count alone: a different encoding name,
/// and a genuinely different channel count, are still re-binds.
#[test]
fn a_real_rebinding_of_an_audio_payload_type_still_fires() {
    for (first, then) in [("PCMA/8000", "PCMU/8000"), ("PCMA/8000", "PCMA/8000/2")] {
        let f = violations(&PayloadTypeMappingStable, &pt8_stream(first, then));
        assert_eq!(f.len(), 1, "{first} then {then}: {f:?}");
        let Decision::Violated(Evidence::PayloadTypeRemapped {
            payload_types,
            prior_encodings,
            encodings,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(payload_types, &vec!["8".to_string()]);
        assert_eq!(prior_encodings, &vec![first.to_string()], "the finding quotes the wire");
        assert_eq!(encodings, &vec![then.to_string()], "the finding quotes the wire");
    }
}

/// The encodings are compared as the block under judgement spells them, so
/// one text repeated across media types stays one binding.
#[test]
fn one_encoding_text_across_media_types_is_one_binding() {
    let offer = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 99\r\n\
a=rtpmap:99 X-FOO/8000\r\n\
m=video 49172 RTP/AVP 99\r\n\
a=rtpmap:99 X-FOO/8000\r\n";
    let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(offer))];
    assert!(violations(&PayloadTypeMappingStable, &msgs).is_empty());
}

/// The default is audio's alone (RFC 4566 §6): outside audio the third
/// subfield carries codec parameters, so no reading makes it a `1`.
#[test]
fn the_channel_count_default_is_not_read_outside_audio() {
    let video = |enc: &str| {
        format!(
            "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 {enc}\r\n"
        )
    };
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(&video("H264/90000"))),
        req(2, ALICE, BOB, "INVITE", 2, Some("bt"), Some(&video("H264/90000/1"))),
    ];
    assert_eq!(violations(&PayloadTypeMappingStable, &msgs).len(), 1);
}

// ---- the two per-description obligations ----------------------------

/// An INVITE alice sent, its body declared under `content_type`.
fn declared(body: &str, content_type: &str) -> Msg {
    let mut m = req(1, ALICE, BOB, "INVITE", 1, None, Some(body));
    m.head = Some(format!("Content-Type: {content_type}\r\n\r\n").into_bytes());
    m
}

const GOOD_SDP: &str = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=ptime:20\r\n";

#[test]
fn a_well_formed_description_is_compliant() {
    let f = run(&SdpBodyParseable, &[declared(GOOD_SDP, "application/sdp")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The `o=` fields are digit strings of any width: an NTP-magnitude
/// `sess-version` is what RFC 4566 §5.2 recommends, not a defect.
#[test]
fn a_sixty_four_bit_session_version_is_compliant() {
    let body = "v=0\r\no=- 2170552860 2736569745311754808 IN IP4 203.0.113.17\r\ns=-\r\n\
                    c=IN IP4 203.0.113.17\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
    let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The grammar walk's own failures, each surfacing verbatim.
#[test]
fn every_grammar_failure_is_named_on_its_message() {
    let cases = [
        (
            "v=0\r\nv=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\n",
            "session descriptions",
        ),
        (
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=ptime:0\r\n",
            "ptime",
        ),
        (
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
            "no c= line",
        ),
    ];
    for (body, needle) in cases {
        let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
        let Decision::Violated(Evidence::SdpBodyRejected { sdp_failure, .. }) = &f[0].decision
        else {
            panic!("sdp evidence: {:?}", f[0].decision)
        };
        assert!(sdp_failure.contains(needle), "{sdp_failure} lacks {needle}");
    }
}

/// A body under another format is not this rule's to read, and neither is a
/// message that carried none.
#[test]
fn an_undeclared_or_bodiless_message_is_no_occasion() {
    assert!(run(&SdpBodyParseable, &[declared("not sdp at all", "text/plain")]).is_empty());
    assert!(run(&SdpBodyParseable, &[declared("", "application/sdp")]).is_empty());
    let mut headless = declared(GOOD_SDP, "application/sdp");
    headless.head = None;
    assert!(run(&SdpBodyParseable, &[headless]).is_empty());
}

/// Held (`c=0.0.0.0`) AND rejected (port 0) at once, at session level and
/// at media level.
#[test]
fn a_held_and_rejected_stream_is_violated() {
    let session = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                       m=audio 0 RTP/AVP 0\r\n";
    let media = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                     m=audio 0 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\n";
    for body in [session, media] {
        let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
        let Decision::Violated(Evidence::HeldAndRejectedStreams {
            held_stream_indexes,
            held_streams,
            held_c_lines,
            ..
        }) = &f[0].decision
        else {
            panic!("held/rejected evidence: {:?}", f[0].decision)
        };
        assert_eq!(held_stream_indexes.as_slice(), [0]);
        assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP"]);
        assert!(held_c_lines[0].contains("0.0.0.0"), "{:?}", held_c_lines);
    }
}

/// Held with a live port, rejected at a real address, and a block whose own
/// `c=` overrides an unspecified session one — none is the ambiguity.
#[test]
fn one_disposition_at_a_time_is_compliant() {
    let bodies = [
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 49170 RTP/AVP 0\r\n",
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\n",
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\nc=IN IP4 10.0.0.1\r\n",
    ];
    for body in bodies {
        let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{body}: {:?}", f[0].decision);
    }
}

/// Two contradictory streams in one body are ONE finding naming both.
#[test]
fn every_offending_stream_rides_one_finding() {
    let body = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                    m=audio 0 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
    let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
    assert_eq!(f.len(), 1, "one description, one finding: {f:?}");
    let Decision::Violated(Evidence::HeldAndRejectedStreams {
        held_stream_indexes,
        held_streams,
        ..
    }) = &f[0].decision
    else {
        panic!("held/rejected evidence: {:?}", f[0].decision)
    };
    assert_eq!(held_stream_indexes.as_slice(), [0, 1]);
    assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP", "video 0 RTP/AVP"]);
}

/// A message carrying no description at all opens no occasion.
#[test]
fn a_bodiless_message_is_no_c0_occasion() {
    assert!(run(&C0PortNonZero, &[req(1, ALICE, BOB, "BYE", 2, Some("bt"), None)]).is_empty());
}
