//! An agent's later descriptions against its earlier ones.

use super::*;

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

// ---- the stream: per dialog, offer/answer methods only --------------

/// `msg` on the dialog whose remote tag is `to_tag`.
fn on_dialog(msg: Msg, to_tag: &str) -> Msg {
    Msg { to_tag: Some(to_tag.to_string()), ..msg }
}

/// `msg` sent inside dialog `tag` by the callee: From carries its tag, To the
/// caller's (RFC 3261 §12.2.1.1).
fn callee_side(msg: Msg, tag: &str) -> Msg {
    Msg { from_tag: Some(tag.to_string()), to_tag: Some("at".to_string()), ..msg }
}

/// A reliable provisional (RFC 3262 §3) to the INVITE on dialog `to_tag`.
fn reliable_183(at_us: u64, to_tag: &str, body: &str) -> Msg {
    Msg {
        head: Some(head("Require: 100rel\r\nRSeq: 1\r\n", Some(body))),
        ..resp_fork(at_us, 183, 1, to_tag, Some(body))
    }
}

/// The callee's answer on the dialog the 2xx confirms: a session of its own,
/// unrelated to the one the early dialog described.
const ANSWER_ON_Y: &str = "v=0\r\n\
o=bob 550055 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60790 RTP/AVP 8\r\n";

/// INVITE(offer) → 183(answer) on early dialog `x` → 200(answer) on dialog
/// `y` → ACK on `y`: one UAS answering one INVITE on two dialogs.
fn forked_answer() -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp_fork(2, 183, 1, "x", Some(AUDIO_ANSWER)),
        resp_fork(3, 200, 1, "y", Some(ANSWER_ON_Y)),
        req(4, ALICE, BOB, "ACK", 1, Some("y"), None),
    ]
}

/// RFC 3261 §12 / §13.2.1: each dialog an INVITE creates carries its own
/// session, so the descriptions an agent sends on two forked dialogs are two
/// streams, and neither continues the other.
#[test]
fn each_forked_dialog_is_its_own_session() {
    let f = violations(&SdpOriginContinuity, &forked_answer());
    assert!(f.is_empty(), "two dialogs, two sessions: {f:?}");
}

/// The caller re-offers on early dialog `x` (UPDATE, RFC 3311 §5.1), then on
/// dialog `y` re-INVITEs with `y_reoffer`. Its INVITE is the prior of both
/// dialogs; the bodies alice sends on `x` belong to `x` alone.
fn caller_on_two_dialogs(y_reoffer: &str) -> Vec<Msg> {
    const UPDATE_ON_X: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
    const UPDATE_ANSWER_ON_X: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n\
a=recvonly\r\n";
    const T38_ANSWER_ON_Y: &str = "v=0\r\n\
o=bob 550055 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60790 udptl t38\r\n";
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        reliable_183(2, "x", AUDIO_ANSWER),
        req(3, ALICE, BOB, "PRACK", 2, Some("x"), None),
        on_dialog(resp(4, BOB, ALICE, 200, 2, "PRACK", None), "x"),
        req(5, ALICE, BOB, "UPDATE", 3, Some("x"), Some(UPDATE_ON_X)),
        on_dialog(resp(6, BOB, ALICE, 200, 3, "UPDATE", Some(UPDATE_ANSWER_ON_X)), "x"),
        resp_fork(7, 200, 1, "y", Some(ANSWER_ON_Y)),
        req(8, ALICE, BOB, "ACK", 1, Some("y"), None),
        req(9, ALICE, BOB, "INVITE", 4, Some("y"), Some(y_reoffer)),
        on_dialog(resp(10, BOB, ALICE, 200, 4, "INVITE", Some(T38_ANSWER_ON_Y)), "y"),
        req(11, ALICE, BOB, "ACK", 4, Some("y"), None),
    ]
}

/// On dialog `y` the caller's re-offer continues its INVITE's `o=` (+1),
/// whatever it sent on the sibling dialog `x`.
#[test]
fn a_re_offer_on_a_forked_dialog_continues_the_invite() {
    let f = violations(&SdpOriginContinuity, &caller_on_two_dialogs(T38_OFFER));
    assert!(f.is_empty(), "{f:?}");
}

/// The sibling's revisions are no step of `y`: a re-offer numbered after
/// `x`'s UPDATE skips a version of the session `y` carries.
#[test]
fn a_forked_dialog_does_not_inherit_its_siblings_revisions() {
    let continues_x = T38_OFFER.replace("o=alice 424242 2", "o=alice 424242 3");
    let f: Vec<Finding> = violations(&SdpOriginContinuity, &caller_on_two_dialogs(&continues_x))
        .into_iter()
        .filter(|f| f.emitter == ALICE)
        .collect();
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 8, "the re-INVITE on y");
    let Decision::Violated(Evidence::SdpOriginDiverged {
        prior_origin_msg,
        same_session,
        session_version,
        prior_session_version,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*prior_origin_msg, 0, "judged against the INVITE, y's prior");
    assert!(same_session);
    assert_eq!((*prior_session_version, *session_version), (1, 3));
}

/// A request the callee sends inside dialog `y` swaps the tags (From `y`, To
/// `at`), and it still continues the callee's stream on `y`.
fn callee_reoffer_on_y(reoffer: &str) -> Vec<Msg> {
    const ALICE_ANSWER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=recvonly\r\n";
    let mut msgs = forked_answer();
    msgs.push(callee_side(req(5, BOB, ALICE, "INVITE", 10, None, Some(reoffer)), "y"));
    msgs.push(callee_side(resp(6, ALICE, BOB, 200, 10, "INVITE", Some(ALICE_ANSWER)), "y"));
    msgs.push(callee_side(req(7, BOB, ALICE, "ACK", 10, None, None), "y"));
    msgs
}

#[test]
fn the_callees_in_dialog_re_offer_continues_its_dialogs_session() {
    let held = ANSWER_ON_Y.replace("o=bob 550055 0", "o=bob 550055 1") + "a=sendonly\r\n";
    let f = violations(&SdpOriginContinuity, &callee_reoffer_on_y(&held));
    assert!(f.is_empty(), "+1 on y's own session: {f:?}");

    let stale = ANSWER_ON_Y.to_string() + "a=sendonly\r\n";
    let f = violations(&SdpOriginContinuity, &callee_reoffer_on_y(&stale));
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (4, BOB), "the callee's re-INVITE");
    let Decision::Violated(Evidence::SdpOriginDiverged { prior_origin_msg, same_session, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*prior_origin_msg, 2, "the 2xx that answered on y");
    assert!(same_session, "same session, changed body under an unchanged version");
}

/// On ONE dialog, an agent that moves to another session's `o=` still breaks
/// §8 — here the callee re-answers on `y` with the session it described on
/// the early dialog `x`.
#[test]
fn a_switch_to_another_session_on_one_dialog_stays_flagged() {
    // T38_ANSWER continues x's session: `o=bob 117241 1`.
    let mut msgs = forked_answer();
    msgs.push(req(5, ALICE, BOB, "INVITE", 3, Some("y"), Some(T38_OFFER)));
    msgs.push(on_dialog(resp(6, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)), "y"));
    msgs.push(req(7, ALICE, BOB, "ACK", 3, Some("y"), None));
    let f = violations(&SdpOriginContinuity, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (5, BOB));
    let Decision::Violated(Evidence::SdpOriginDiverged {
        prior_origin_msg,
        same_session,
        origin_line,
        prior_origin_line,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*prior_origin_msg, 2);
    assert!(!same_session);
    assert_eq!(origin_line, "o=bob 117241 1 IN IP4 10.0.0.2");
    assert_eq!(prior_origin_line, "o=bob 550055 0 IN IP4 10.0.0.2");
}

/// A capability set (RFC 3261 §11.2): another `o=`, fewer `m=` lines, and
/// payload type 96 bound to another encoding.
const CAPABILITIES: &str = "v=0\r\n\
o=bob 1 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 0 96\r\n\
a=rtpmap:96 telephone-event/8000\r\n";

/// The caller's own capability query, carrying a body (RFC 3261 §11.1).
const CALLER_CAPABILITIES: &str = "v=0\r\n\
o=alice 7 7 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 0 8\r\n";

/// The audio call, an in-dialog OPTIONS and its 200 both carrying bodies,
/// then a T.38 re-INVITE whose offer is `reoffer`.
fn options_mid_call(reoffer: &str) -> Vec<Msg> {
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "OPTIONS", 2, Some("bt"), Some(CALLER_CAPABILITIES)),
        resp(5, BOB, ALICE, 200, 2, "OPTIONS", Some(CAPABILITIES)),
        req(6, ALICE, BOB, "INVITE", 3, Some("bt"), Some(reoffer)),
        resp(7, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)),
        req(8, ALICE, BOB, "ACK", 3, Some("bt"), None),
    ]
}

/// An OPTIONS body is a capability set, no offer, no answer: neither a prior
/// nor a successor of the session stream.
#[test]
fn an_options_body_is_no_step_of_the_session_stream() {
    let out = run(&SdpOriginContinuity, &options_mid_call(T38_OFFER));
    assert!(out.iter().all(|f| f.anchor != 3 && f.anchor != 4), "{out:?}");
    let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
    assert!(f.is_empty(), "{f:?}");
}

/// Across an OPTIONS exchange, a re-offer is judged against the dialog's last
/// session description.
#[test]
fn a_re_offer_after_an_options_is_judged_against_the_last_session_description() {
    let stale = T38_OFFER.replace("o=alice 424242 2", "o=alice 424242 1");
    let f = violations(&SdpOriginContinuity, &options_mid_call(&stale));
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].anchor, f[0].emitter.as_str()), (5, ALICE));
    let Decision::Violated(Evidence::SdpOriginDiverged {
        prior_origin_msg,
        same_session,
        body_changed,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision);
    };
    assert_eq!(*prior_origin_msg, 0, "the INVITE, not the OPTIONS");
    assert!(same_session);
    assert!(body_changed);
}

/// A failure final's body lists capabilities (RFC 3261 §21.4.26), no answer:
/// the callee's next answer continues its session from the 2xx before it.
#[test]
fn a_failure_final_body_is_no_step_of_the_session_stream() {
    let msgs = vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        req(4, ALICE, BOB, "INVITE", 2, Some("bt"), Some(T38_OFFER)),
        resp(5, BOB, ALICE, 488, 2, "INVITE", Some(CAPABILITIES)),
        req(6, ALICE, BOB, "ACK", 2, Some("bt"), None),
        req(7, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
        resp(8, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)),
        req(9, ALICE, BOB, "ACK", 3, Some("bt"), None),
    ];
    for rule in [
        &SdpOriginContinuity as &dyn Obligation,
        &ReOfferMLineCountMonotonic,
        &PayloadTypeMappingStable,
    ] {
        let out = run(rule, &msgs);
        assert!(out.iter().all(|f| f.anchor != 4), "{out:?}");
        let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
        assert!(f.is_empty(), "{f:?}");
    }
}

/// The two-media call, an OPTIONS whose 200 carries the capability set, then
/// a re-INVITE re-stating both streams.
fn options_mid_two_media_call() -> Vec<Msg> {
    let mut msgs = one_round(OFFER_2MEDIA, ANSWER_2MEDIA);
    msgs.push(req(4, ALICE, BOB, "OPTIONS", 2, Some("bt"), None));
    msgs.push(resp(5, BOB, ALICE, 200, 2, "OPTIONS", Some(CAPABILITIES)));
    msgs.push(req(6, ALICE, BOB, "INVITE", 3, Some("bt"), Some(OFFER_2MEDIA)));
    msgs.push(resp(7, BOB, ALICE, 200, 3, "INVITE", Some(ANSWER_2MEDIA)));
    msgs.push(req(8, ALICE, BOB, "ACK", 3, Some("bt"), None));
    msgs
}

#[test]
fn an_options_body_is_no_step_of_the_stream_table() {
    let out = run(&ReOfferMLineCountMonotonic, &options_mid_two_media_call());
    assert!(out.iter().all(|f| f.anchor != 4), "{out:?}");
    let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
    assert!(f.is_empty(), "{f:?}");
}

#[test]
fn an_options_body_binds_no_payload_type() {
    let out = run(&PayloadTypeMappingStable, &options_mid_two_media_call());
    assert!(out.iter().all(|f| f.anchor != 4), "{out:?}");
    let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
    assert!(f.is_empty(), "{f:?}");
}

/// The caller adds video on early dialog `x` (UPDATE), and on dialog `y`
/// re-offers audio alone with payload type 98 bound to another encoding than
/// `x` bound it to. Each dialog's table and bindings start from the INVITE.
fn caller_tables_on_two_dialogs() -> Vec<Msg> {
    const VIDEO_ON_X: &str = "v=0\r\n\
o=alice 1 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 98\r\n\
a=rtpmap:98 H264/90000\r\n\
a=sendrecv\r\n";
    const VIDEO_ANSWER_ON_X: &str = "v=0\r\n\
o=bob 2 3 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 98\r\n\
a=rtpmap:98 H264/90000\r\n\
a=sendrecv\r\n";
    const ANSWER_1AUDIO_ON_Y: &str = "v=0\r\n\
o=bob 5 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50010 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";
    const EVENTS_ON_Y: &str = "v=0\r\n\
o=alice 1 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96 98\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=rtpmap:98 telephone-event/8000\r\n\
a=sendrecv\r\n";
    const EVENTS_ANSWER_ON_Y: &str = "v=0\r\n\
o=bob 5 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50010 RTP/AVP 0 96 98\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=rtpmap:98 telephone-event/8000\r\n\
a=sendrecv\r\n";
    vec![
        req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
        reliable_183(2, "x", ANSWER_1AUDIO),
        req(3, ALICE, BOB, "PRACK", 2, Some("x"), None),
        on_dialog(resp(4, BOB, ALICE, 200, 2, "PRACK", None), "x"),
        req(5, ALICE, BOB, "UPDATE", 3, Some("x"), Some(VIDEO_ON_X)),
        on_dialog(resp(6, BOB, ALICE, 200, 3, "UPDATE", Some(VIDEO_ANSWER_ON_X)), "x"),
        resp_fork(7, 200, 1, "y", Some(ANSWER_1AUDIO_ON_Y)),
        req(8, ALICE, BOB, "ACK", 1, Some("y"), None),
        req(9, ALICE, BOB, "INVITE", 4, Some("y"), Some(EVENTS_ON_Y)),
        on_dialog(resp(10, BOB, ALICE, 200, 4, "INVITE", Some(EVENTS_ANSWER_ON_Y)), "y"),
        req(11, ALICE, BOB, "ACK", 4, Some("y"), None),
    ]
}

#[test]
fn a_forked_siblings_stream_table_is_no_step_of_the_dialog() {
    let f = violations(&ReOfferMLineCountMonotonic, &caller_tables_on_two_dialogs());
    assert!(f.is_empty(), "{f:?}");
}

#[test]
fn a_forked_siblings_bindings_do_not_bind_the_dialog() {
    let f = violations(&PayloadTypeMappingStable, &caller_tables_on_two_dialogs());
    assert!(f.is_empty(), "{f:?}");
}
