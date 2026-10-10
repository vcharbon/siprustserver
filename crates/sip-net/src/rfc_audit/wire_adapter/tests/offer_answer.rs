//! The live surfacing of the offer/answer family rules.

use super::*;

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

/// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
/// 200(answer) → ACK, all from the CALLER's bind. The ACK bodies are the
/// caller's choice.
fn t38_call(
    first_ack: Option<&str>,
    second_ack: Option<&str>,
) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        recv(ALICE, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), BOB, 1),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 2),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), first_ack), BOB, 3),
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 4),
        recv(ALICE, resp_sdp(200, 3, "INVITE", "z9hG4bK-i3", Some(T38_ANSWER)), BOB, 5),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a3", 3, Some("bt"), second_ack), BOB, 6),
    ]
}

#[test]
fn an_ack_body_on_a_completed_round_is_flagged_at_its_wire_position() {
    assert!(live(RuleId::AckBodyAfterCompleteOfferAnswer).check(&t38_call(None, None)).is_empty());

    let out = live(RuleId::AckBodyAfterCompleteOfferAnswer)
        .check_positioned(&t38_call(None, Some(STRAY_ANSWER)));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "charged to the lane that sent the ACK");
    assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§13.2.2.4"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(7), "offending points at the ACK that carried the body");

    // The establishing round is closed by the 180, not by the bodyless 200.
    let first =
        live(RuleId::AckBodyAfterCompleteOfferAnswer).check(&t38_call(Some(STRAY_ANSWER), None));
    assert_eq!(first.len(), 1, "{first:?}");
}

#[test]
fn a_delayed_offer_ack_carries_the_answer_and_is_clean() {
    // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
    let evs = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), Some(AUDIO_ANSWER)), BOB, 2),
    ];
    assert!(live(RuleId::AckBodyAfterCompleteOfferAnswer).check(&evs).is_empty());
}

/// The same as [`resp_sdp`], made reliable: `Require: 100rel` and an
/// `RSeq`, so its description binds (RFC 3262 §5).
fn reliable_resp_sdp(status: u16, cseq: u32, branch: &str, body: Option<&str>) -> Vec<u8> {
    let plain = resp_sdp(status, cseq, "INVITE", branch, body);
    let text = String::from_utf8(plain).unwrap();
    text.replacen("CSeq:", "Require: 100rel\r\nRSeq: 1\r\nCSeq:", 1).into_bytes()
}

/// §13.2.2.4 GATES: an ACK to a delayed-offer 2xx that carries no answer
/// is flagged at the 2xx sender's lane, charged to the ACK's sender.
#[test]
fn a_delayed_offer_acked_without_an_answer_gates() {
    assert!(!live(RuleId::DelayedOfferAnsweredInAck).force_advisory(), "a MUST gates");
    let answered = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), Some(AUDIO_ANSWER)), ALICE, 2),
    ];
    assert!(live(RuleId::DelayedOfferAnsweredInAck).check(&answered).is_empty());
    let silent = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = live(RuleId::DelayedOfferAnsweredInAck).check_positioned(&silent);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported where the answer had to arrive");
    assert_eq!(out[0].3, Some(ALICE.to_string()), "charged to the ACK's sender");
}

#[test]
fn a_final_leaving_the_offer_unanswered_is_flagged_at_its_wire_position() {
    assert!(!live(RuleId::Final2xxAnswersTheOffer).force_advisory(), "a MUST gates");

    // Judged from the ANSWERER's bind: it takes the offer and ends the
    // round with a 200 stating no plan of its own.
    let clean = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(live(RuleId::Final2xxAnswersTheOffer).check(&clean).is_empty());

    let silent = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = live(RuleId::Final2xxAnswersTheOffer).check_positioned(&silent);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported on the answerer's own lane");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer");
    assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§13.2.1"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the 200 that answered nothing");
}

#[test]
fn a_reliable_provisional_answer_discharges_the_final() {
    let evs = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, reliable_resp_sdp(183, 1, "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("PRACK", "z9hG4bK-p1", 2, Some("bt"), None), ALICE, 2),
        sent(BOB, resp_sdp(200, 2, "PRACK", "z9hG4bK-p1", None), ALICE, 3),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
    ];
    assert!(
        live(RuleId::Final2xxAnswersTheOffer).check(&evs).is_empty(),
        "RFC 3262 §5 answered it"
    );
}

#[test]
fn a_silent_answerer_is_named_on_the_lane_that_took_it() {
    // The callee is no recorded bind; the caller's lane is where the
    // recording checks the round at all.
    let evs = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
    ];
    let out = live(RuleId::Final2xxAnswersTheOffer).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "reported at the vantage that took the 200");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer all the same");
}

#[test]
fn a_failure_final_and_a_cancelled_round_owe_no_answer() {
    let rejected = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(486, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(live(RuleId::Final2xxAnswersTheOffer).check(&rejected).is_empty());

    let cancelled = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("CANCEL", "z9hG4bK-i1", 1, None, None), ALICE, 2),
        sent(BOB, resp_sdp(200, 1, "CANCEL", "z9hG4bK-i1", None), ALICE, 3),
        sent(BOB, resp_sdp(487, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
    ];
    assert!(live(RuleId::Final2xxAnswersTheOffer).check(&cancelled).is_empty());
}

#[test]
fn an_answer_re_typing_the_offered_stream_is_flagged_on_the_answerer() {
    // Judged from the ANSWERER's bind: it takes the offer and sends back a
    // stream of another type.
    let clean = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(T38_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(live(RuleId::AnswerStreamMatchesOffer).check(&clean).is_empty());

    let retyped = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = live(RuleId::AnswerStreamMatchesOffer).check_positioned(&retyped);
    assert_eq!(out.len(), 1, "one answer is one finding: {out:?}");
    assert_eq!(out[0].0, BOB);
    assert!(out[0].1.contains("m=[0]"), "{}", out[0].1);
    assert!(out[0].1.contains("image 27500 udptl"), "{}", out[0].1);
    assert!(out[0].1.contains("audio 60788 RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§6"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the answering 200");
}

#[test]
fn an_origin_that_leaves_the_session_is_advisory_and_positioned() {
    assert!(live(RuleId::SdpOriginContinuity).force_advisory());
    assert_eq!(
        live(RuleId::SdpOriginContinuity).subject(),
        HashSet::from([UaRole::Uac, UaRole::Uas])
    );
    assert!(live(RuleId::SdpOriginContinuity).check(&t38_call(None, None)).is_empty());

    let out =
        live(RuleId::SdpOriginContinuity).check_positioned(&t38_call(None, Some(STRAY_ANSWER)));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("o=bob 1 1 IN IP4 127.0.0.1"), "{}", out[0].1);
    assert!(out[0].1.contains("o=alice 424242 2 IN IP4 10.0.0.1"), "{}", out[0].1);
    assert!(out[0].1.contains("different session"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(7), "offending points at the ACK carrying the foreign origin");
}

#[test]
fn a_relay_lane_carrying_both_agents_origins_is_not_judged() {
    // A transparent proxy forwards alice's offer AND bob's answer on one
    // bind, so its sent stream interleaves o=alice and o=bob for one
    // Call-ID. That is the two agents' continuity, never the relay's.
    let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
    let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER));
    let evs = vec![
        recv(SUT, inv.clone(), ALICE, 0),
        sent(SUT, inv, BOB, 1),
        recv(SUT, ok.clone(), BOB, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    assert!(
        live(RuleId::SdpOriginContinuity).check(&evs).is_empty(),
        "{:?}",
        live(RuleId::SdpOriginContinuity).check(&evs)
    );
    assert!(live(RuleId::AnswerStreamMatchesOffer).check(&evs).is_empty());
    assert!(live(RuleId::AckBodyAfterCompleteOfferAnswer).check(&evs).is_empty());
}

// ── the RFC 3264 offer/answer-model rules ───────────────────────────────

/// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

/// An answer wrong four ways at once: an added stream, a rewritten `t=`,
/// video where audio was offered, and a bare port-0 rejection.
const BAD_ANSWER: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=100 200\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n\
m=audio 0 RTP/AVP\r\n";

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

/// INVITE(offer) → 200(answer) → ACK on the CALLER's bind: the caller sends
/// the offer and TAKES the answer, so the answerer is no recorded bind.
fn sdp_round(offer: &str, answer: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(offer)), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(answer)), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
    ]
}

#[test]
fn a_second_offer_over_an_unanswered_one_is_advisory_at_either_end() {
    assert!(live(RuleId::NoNewOfferWhileOfferPending).force_advisory());
    assert!(live(RuleId::NoNewOfferWhileOfferPending).check(&t38_call(None, None)).is_empty());

    let glare = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 1),
    ];
    let out = live(RuleId::NoNewOfferWhileOfferPending).check_positioned(&glare);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "charged to the lane that sent the second offer");
    assert!(out[0].1.contains("MUST-002"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the second offer");

    // The same act at the lane that only TOOK the two offers: the offender
    // is no recorded bind, and the recording is where it is checked at all.
    let taker = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), ALICE, 1),
    ];
    let out = live(RuleId::NoNewOfferWhileOfferPending).check(&taker);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported on the lane that took it");
}

#[test]
fn a_bad_answer_is_named_at_the_offerers_lane() {
    assert!(live(RuleId::AnswerMLineCountMatchesOffer)
        .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
        .is_empty());

    let evs = sdp_round(OFFER_1AUDIO, BAD_ANSWER);
    for (rule, must, quote) in [
        (
            Box::new(live(RuleId::AnswerMLineCountMatchesOffer)) as Box<dyn CrossMessageAuditRule>,
            "MUST-018",
            "video/RTP/AVP",
        ),
        (Box::new(live(RuleId::AnswerTLineEqualsOffer)), "MUST-019", "100 200"),
        (Box::new(live(RuleId::AnswerMediaTypeMatchesOffer)), "MUST-022", "m=[0]"),
        (Box::new(live(RuleId::RejectedStreamMinimalAnswer)), "MUST-021", "m=[1]"),
    ] {
        let out = rule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{}: {out:?}", rule.name());
        assert_eq!(out[0].0, ALICE, "{}: reported on the lane that took it", rule.name());
        assert!(out[0].1.contains(must), "{}: {}", rule.name(), out[0].1);
        assert!(out[0].1.contains(quote), "{}: {}", rule.name(), out[0].1);
        assert_eq!(out[0].2, Some(2), "{}: offending points at the answer", rule.name());
    }

    // The answerer's OWN lane names it too — it is the charged party.
    let at_answerer = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = live(RuleId::AnswerMLineCountMatchesOffer).check(&at_answerer);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB);
}

#[test]
fn the_direction_and_zero_port_pairings_are_advisory() {
    assert!(live(RuleId::DirectionPairValid).force_advisory());
    assert!(live(RuleId::ZeroPortPropagation).force_advisory());

    let held = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
    let out = live(RuleId::DirectionPairValid).check_positioned(&sdp_round(&held, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-023"), "{}", out[0].1);
    assert!(out[0].1.contains("answer 'sendrecv' invalid for offer 'inactive'"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2));

    let disabled = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    assert!(live(RuleId::DirectionPairValid)
        .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
        .is_empty());
    let out =
        live(RuleId::ZeroPortPropagation).check_positioned(&sdp_round(&disabled, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-044"), "{}", out[0].1);
    assert!(out[0].1.contains("m=[0] answered port=50000"), "{}", out[0].1);
}

#[test]
fn a_shrinking_re_offer_is_charged_only_to_the_lane_that_sent_it() {
    let mut evs = sdp_round(OFFER_2MEDIA, ANSWER_2MEDIA);
    evs.push(sent(
        ALICE,
        req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)),
        BOB,
        3,
    ));
    let out = live(RuleId::ReOfferMLineCountMonotonic).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("MUST-042"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the shrinking re-offer");

    // The lane that only TOOK that stream authored nothing: the rule judges
    // what a lane SENT, so it is charged at the sender's vantage alone.
    let taker = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_2MEDIA)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(ANSWER_2MEDIA)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)), ALICE, 3),
    ];
    assert!(live(RuleId::ReOfferMLineCountMonotonic).check(&taker).is_empty());
}

#[test]
fn a_rebound_payload_type_is_named_at_either_end() {
    assert!(live(RuleId::PayloadTypeMappingStable)
        .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
        .is_empty());

    let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
    let mut evs = sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    evs.push(sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(&reoffer)), BOB, 3));
    let out = live(RuleId::PayloadTypeMappingStable).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("MUST-047"), "{}", out[0].1);
    assert!(
        out[0].1.contains("payload-type 96 was 'opus/48000/2' now 'H264/90000'"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(4));
}

#[test]
fn a_relay_lane_carrying_a_negotiation_is_not_judged_on_it() {
    // The relay forwards both agents' descriptions on one Call-ID and has
    // no offer/answer state of its own, so the bad answer it passes through
    // is the upstream's and is flagged on the upstream lane.
    let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO));
    let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER));
    let evs = vec![
        recv(SUT, inv.clone(), ALICE, 0),
        sent(SUT, inv, BOB, 1),
        recv(SUT, ok.clone(), BOB, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    let rules: Vec<Box<dyn CrossMessageAuditRule>> = vec![
        Box::new(live(RuleId::NoNewOfferWhileOfferPending)),
        Box::new(live(RuleId::AnswerMLineCountMatchesOffer)),
        Box::new(live(RuleId::AnswerTLineEqualsOffer)),
        Box::new(live(RuleId::AnswerMediaTypeMatchesOffer)),
        Box::new(live(RuleId::DirectionPairValid)),
        Box::new(live(RuleId::RejectedStreamMinimalAnswer)),
        Box::new(live(RuleId::ReOfferMLineCountMonotonic)),
        Box::new(live(RuleId::ZeroPortPropagation)),
        Box::new(live(RuleId::PayloadTypeMappingStable)),
    ];
    for rule in rules {
        let out = rule.check(&evs);
        assert!(out.is_empty(), "{}: {out:?}", rule.name());
    }
}
