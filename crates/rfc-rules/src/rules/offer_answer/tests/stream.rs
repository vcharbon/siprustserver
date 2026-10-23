//! The session stream an agent's later descriptions are judged on: one per
//! dialog, offer/answer bodies only.

use super::*;

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

/// An element that forwards the INVITE and the callee's provisionals sends
/// both, but is the caller of no dialog the provisionals ride: the INVITE's
/// description is no prior of the ones it forwards back.
#[test]
fn a_forwarded_response_continues_no_request_of_its_sender() {
    const RELAY: &str = "10.0.0.9:5060";
    let msgs = vec![
        req(1, RELAY, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
        Msg {
            src: RELAY.to_string(),
            dst: ALICE.to_string(),
            ..resp_fork(2, 183, 1, "x", Some(AUDIO_ANSWER))
        },
        Msg {
            src: RELAY.to_string(),
            dst: ALICE.to_string(),
            ..resp_fork(3, 183, 1, "y", Some(ANSWER_ON_Y))
        },
    ];
    let f = violations(&SdpOriginContinuity, &msgs);
    assert!(f.is_empty(), "{f:?}");
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
