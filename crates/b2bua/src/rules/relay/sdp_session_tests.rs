//! The seam's decisions on sequences of descriptions crossing one dialog,
//! each named by the leg its author spoke on.

use call::{Call, LegState};
use sip_message::header::MediaType;
use sip_message::{Method, SipStr};

use super::{continue_on_leg, Author, Carried};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

/// A confirmed caller leg `a` and confirmed legs `b-1`, `b-2`, `b-3`.
fn call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "sdp"),
        src(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.a_leg.state = LegState::Confirmed;
    for id in ["b-1", "b-2", "b-3"] {
        let mut leg = call.a_leg.clone();
        leg.leg_id = id.into();
        call.b_legs.push(leg);
    }
    call
}

fn sdp(origin: &str, port: u16) -> String {
    format!(
        "v=0\r\no={origin}\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 0\r\n"
    )
}

fn ct() -> MediaType {
    MediaType::new(SipStr::from_static("application/sdp"))
}

/// What reaches leg `to` of `body` written by `author`, `carried` as stated.
fn send(call: &mut Call, to: &str, author: Author<'_>, carried: Carried, body: &str) -> String {
    let out = continue_on_leg(call, to, author, carried, body.as_bytes().to_vec(), Some(&ct()));
    String::from_utf8(out).unwrap()
}

fn o_line(body: &str) -> &str {
    body.split("\r\n").find(|l| l.starts_with("o=")).unwrap()
}

/// The dialog's own author, once the stack has restated a version of its
/// session, no longer continues the versions the far party holds: its next
/// description, same sess-id, is restated above the stack's.
#[test]
fn the_authors_return_after_a_restatement_is_restated_above_it() {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
    );
    let spliced = send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &sdp("carol 303 1 IN IP4 192.0.2.3", 30000),
    );
    assert_eq!(o_line(&spliced), "o=bob 202 2 IN IP4 192.0.2.2");
    let back = send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::InDialog,
        &sdp("bob 202 2 IN IP4 192.0.2.2", 20002),
    );
    assert_eq!(o_line(&back), "o=bob 202 3 IN IP4 192.0.2.2", "never version 2 twice");
}

/// Authors sharing a sess-id (`o=- 0 0`) are told apart by the leg they speak
/// on: another leg's description is restated whatever its sess-id.
#[test]
fn another_leg_is_restated_whatever_its_session_id() {
    let mut c = call();
    send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, &sdp("- 0 0 IN IP4 192.0.2.2", 20000));
    let spliced = send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &sdp("- 0 0 IN IP4 192.0.2.3", 30000),
    );
    assert_eq!(o_line(&spliced), "o=- 0 1 IN IP4 192.0.2.2");
}

/// A plain relay stays byte-transparent: the dialog's author under its own
/// sess-id leaves as written, a moved address or a version that did not rise
/// included (the author's own account of its session).
#[test]
fn the_dialogs_own_author_leaves_as_written() {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
    );
    for body in [
        sdp("bob 202 2 IN IP4 192.0.2.2", 20002),
        sdp("sbc 202 3 IN IP4 198.51.100.9", 20004),
        sdp("sbc 202 3 IN IP4 198.51.100.9", 20006),
    ] {
        assert_eq!(send(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &body), body);
    }
}

/// The dialog's author under a new sess-id states another session: restated.
#[test]
fn the_authors_new_session_id_is_restated() {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
    );
    let out = send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::InDialog,
        &sdp("bob 777 1 IN IP4 192.0.2.2", 20002),
    );
    assert_eq!(o_line(&out), "o=bob 202 2 IN IP4 192.0.2.2");
}

/// A description in a response of 300 or more describes capabilities (RFC
/// 3261 §13.2.1): as written, and the dialog's session is untouched by it.
#[test]
fn a_failure_response_body_stays_outside_the_session() {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
    );
    let caps = sdp("gw 909 1 IN IP4 192.0.2.9", 0);
    let carried = Carried::of(&Method::Invite, Some(488), true);
    assert_eq!(send(&mut c, "a", Author::Leg("b-2"), carried, &caps), caps);
    let next = sdp("bob 202 2 IN IP4 192.0.2.2", 20002);
    assert_eq!(send(&mut c, "a", Author::Leg("b-1"), Carried::InDialog, &next), next);
}

/// With three parties sharing one sess-id, the far party's answer goes back
/// in the order of the author restated toward the leg it came from — named by
/// that leg, not guessed from the sess-id.
#[test]
fn the_answer_goes_back_through_the_leg_it_came_from() {
    let mut c = call();
    let av = "v=0\r\no=- 0 0 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=video 20002 RTP/AVP 96\r\n";
    send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, av);
    send(&mut c, "b-3", Author::Leg("a"), Carried::Opening, &sdp("- 0 0 IN IP4 192.0.2.1", 10000));
    let spliced = send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &sdp("- 0 0 IN IP4 192.0.2.3", 30000),
    );
    assert!(spliced.ends_with("m=audio 30000 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n"), "{spliced}");
    let answer = "v=0\r\no=- 0 0 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
    let to_author = send(&mut c, "b-2", Author::Leg("a"), Carried::InDialog, answer);
    assert!(to_author.ends_with("m=audio 10002 RTP/AVP 0\r\n"), "{to_author}");
}
