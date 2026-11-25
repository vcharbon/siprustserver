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

// ---- sdp-origin-continuity: a gap the relay inherited ---------------

/// The B2BUA between ALICE (call `a`) and BOB (call `b`).
const RELAY: &str = "10.0.0.9:5060";

/// ALICE's third revision of her session: the description after the one the
/// relay refused.
const AUDIO_OFFER_V3: &str = "v=0\r\n\
o=alice 424242 3 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27502 RTP/AVP 8\r\n";

fn on_call(mut msg: Msg, call_id: &str) -> Msg {
    msg.call_id = call_id.to_string();
    msg
}

/// ALICE's session relayed to BOB: her offer, then a re-offer (version 2) the
/// relay answers `refusal` on her call, then her next re-offer (version 3),
/// and the description the relay sends BOB after version 1, `onward`.
fn relayed_reoffers(refusal: u16, onward: &str) -> (Vec<Msg>, usize) {
    let msgs = vec![
        on_call(req(1, ALICE, RELAY, "INVITE", 1, None, Some(AUDIO_OFFER)), "a"),
        on_call(req(2, RELAY, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)), "b"),
        on_call(resp(3, BOB, RELAY, 200, 1, "INVITE", Some(AUDIO_ANSWER)), "b"),
        on_call(req(4, RELAY, BOB, "ACK", 1, Some("bt"), None), "b"),
        on_call(resp(5, RELAY, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)), "a"),
        on_call(req(6, ALICE, RELAY, "ACK", 1, Some("bt"), None), "a"),
        on_call(req(7, ALICE, RELAY, "INVITE", 2, Some("bt"), Some(T38_OFFER)), "a"),
        on_call(resp(8, RELAY, ALICE, refusal, 2, "INVITE", None), "a"),
        on_call(req(9, ALICE, RELAY, "ACK", 2, Some("bt"), None), "a"),
        on_call(req(10, ALICE, RELAY, "INVITE", 3, Some("bt"), Some(AUDIO_OFFER_V3)), "a"),
        on_call(req(11, RELAY, BOB, "INVITE", 2, Some("bt"), Some(onward)), "b"),
    ];
    (msgs, 10)
}

fn origin_finding(msgs: &[Msg], anchor: usize) -> Finding {
    run(&SdpOriginContinuity, msgs)
        .into_iter()
        .find(|f| f.anchor == anchor)
        .unwrap_or_else(|| panic!("no finding on message {anchor}"))
}

/// RFC 3264 §8: the version gap on BOB's side is ALICE's own continuous
/// sequence minus the re-offer the relay refused (491, RFC 3261 §14.1); a
/// verbatim relay of her next description inherits it, and the finding is the
/// relay's to forward, not to answer for.
#[test]
fn a_gap_left_by_a_refused_offer_on_the_other_call_is_relayed() {
    let (msgs, onward) = relayed_reoffers(491, AUDIO_OFFER_V3);
    let f = origin_finding(&msgs, onward);
    assert!(f.violated(), "the gap is still on the wire: {f:?}");
    assert!(f.relayed, "a verbatim relay of what the refused offer skipped: {f:?}");
}

/// The relay ANSWERED the middle offer: dropping it toward BOB is its own.
#[test]
fn a_gap_left_by_an_answered_offer_stays_the_relay_s() {
    let (msgs, onward) = relayed_reoffers(200, AUDIO_OFFER_V3);
    let f = origin_finding(&msgs, onward);
    assert!(f.violated() && !f.relayed, "{f:?}");
}

/// A description the relay changed is no relay of ALICE's: its gap is its own.
#[test]
fn a_gap_on_a_description_the_relay_rewrote_stays_the_relay_s() {
    let rewritten = AUDIO_OFFER_V3.replace("27502", "40000");
    let (msgs, onward) = relayed_reoffers(491, &rewritten);
    let f = origin_finding(&msgs, onward);
    assert!(f.violated() && !f.relayed, "{f:?}");
}

/// The relay refused version 2 once, then ACCEPTED it re-offered on a new
/// transaction: the version reached it and was never forwarded, so the gap
/// toward BOB is its own (RFC 3264 §8).
#[test]
fn a_gap_left_by_an_offer_refused_once_then_accepted_stays_the_relay_s() {
    let (mut msgs, _) = relayed_reoffers(491, AUDIO_OFFER_V3);
    let resent = [
        on_call(req(10, ALICE, RELAY, "INVITE", 3, Some("bt"), Some(T38_OFFER)), "a"),
        on_call(resp(11, RELAY, ALICE, 200, 3, "INVITE", Some(AUDIO_ANSWER)), "a"),
        on_call(req(12, ALICE, RELAY, "ACK", 3, Some("bt"), None), "a"),
        on_call(req(13, ALICE, RELAY, "INVITE", 4, Some("bt"), Some(AUDIO_OFFER_V3)), "a"),
        on_call(req(14, RELAY, BOB, "INVITE", 2, Some("bt"), Some(AUDIO_OFFER_V3)), "b"),
    ];
    msgs.truncate(9);
    msgs.extend(resent);
    let f = origin_finding(&msgs, 13);
    assert!(f.violated() && !f.relayed, "{f:?}");
}
