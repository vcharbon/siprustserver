//! The seam's decisions on sequences of descriptions crossing one dialog,
//! each named by the leg its author spoke on.

use call::{Call, LegState};
use sip_message::header::MediaType;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipStr};

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

/// A stated version at the top of its range has no next one: the description
/// leaves as written — opening the session anew where the stack had stated no
/// version of its own, keeping the dialog's session where it had.
#[test]
fn a_version_with_no_next_one_leaves_the_description_as_written() {
    let top = format!("bob 202 {} IN IP4 192.0.2.2", u64::MAX);
    for restated in [false, true] {
        let mut c = call();
        send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, &sdp(&top, 20000));
        c.a_leg.sdp_session.restated = restated;
        let carol = sdp("carol 303 1 IN IP4 192.0.2.3", 30000);
        assert_eq!(send(&mut c, "a", Author::Leg("b-2"), Carried::InDialog, &carol), carol);
        let author = c.a_leg.sdp_session.session_author.as_deref();
        assert_eq!(author, Some(if restated { "b-1" } else { "b-2" }), "restated={restated}");
    }
}

/// The slot map of a restatement only reorders what goes back to the author
/// it was made from: during a hold on a media server, the caller's own
/// re-offer relayed to another leg leaves in the caller's order.
#[test]
fn the_slot_map_only_serves_the_restated_author() {
    let mut c = call();
    let av = "v=0\r\no=bob 202 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=video 20002 RTP/AVP 96\r\n";
    send(&mut c, "a", Author::Leg("b-1"), Carried::Opening, av);
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &sdp("media 404 1 IN IP4 192.0.2.4", 40000),
    );
    let alice = "v=0\r\no=alice 101 2 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
    send(&mut c, "b-1", Author::Leg("a"), Carried::Opening, alice);
    assert_eq!(
        send(&mut c, "b-1", Author::Leg("a"), Carried::InDialog, alice),
        alice,
        "to Bob: as written"
    );
    assert!(
        send(&mut c, "b-2", Author::Leg("a"), Carried::InDialog, alice)
            .ends_with("m=audio 10002 RTP/AVP 0\r\n"),
        "to the media server: its one stream"
    );
}

/// The peer of `leg` sends a `method` request, with or without a
/// description, as the router receives it.
fn peer_sends(call: &mut Call, leg: &str, method: &str, with_sdp: bool) {
    let body = if with_sdp { sdp("x 1 1 IN IP4 192.0.2.9", 1) } else { String::new() };
    let content_type = if with_sdp { "Content-Type: application/sdp\r\n" } else { "" };
    let raw = format!(
        "{method} sip:b2bua@192.0.2.100 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-{method}\r\n\
         From: <sip:a@192.0.2.1>;tag=a\r\nTo: <sip:b@192.0.2.100>;tag=b\r\n\
         Call-ID: c\r\nCSeq: 7 {method}\r\nMax-Forwards: 70\r\n\
         {content_type}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a request")
    };
    super::note_request(call, leg, &req);
}

/// The peer of `leg` opens an offer/answer exchange with a request carrying a
/// description.
fn peer_offers(call: &mut Call, leg: &str, method: Method) {
    peer_sends(call, leg, method.as_str(), true);
}

/// A splice of `b-2` onto `a`, the dialog carrying `b-1`'s session.
fn spliced() -> Call {
    let mut c = call();
    send(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
    );
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &sdp("carol 303 1 IN IP4 192.0.2.3", 30000),
    );
    c
}

/// A 2xx repeating the answer a nested UPDATE exchange already gave answers no
/// new offer (RFC 6337 §3.1): the last restatement, unchanged.
#[test]
fn the_final_after_a_nested_exchange_repeats_its_answer() {
    let mut c = spliced();
    let answering = Carried::answering(&Method::Invite, 183);
    peer_offers(&mut c, "a", Method::Invite);
    let early = send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        answering,
        &sdp("carol 303 4 IN IP4 192.0.2.3", 30002),
    );
    assert_eq!(o_line(&early), "o=bob 202 3 IN IP4 192.0.2.2");
    peer_offers(&mut c, "a", Method::Update);
    let update = Carried::answering(&Method::Update, 200);
    let nested =
        send(&mut c, "a", Author::Leg("b-2"), update, &sdp("carol 303 5 IN IP4 192.0.2.3", 30004));
    assert_eq!(o_line(&nested), "o=bob 202 4 IN IP4 192.0.2.2");
    let fin = Carried::answering(&Method::Invite, 200);
    let final_ =
        send(&mut c, "a", Author::Leg("b-2"), fin, &sdp("carol 303 5 IN IP4 192.0.2.3", 30004));
    assert_eq!(final_, nested, "the same answer, the same version");
}

/// Within one exchange, an author that raised its version described something
/// new: the next version.
#[test]
fn a_raised_version_in_the_final_is_a_new_version() {
    let mut c = spliced();
    peer_offers(&mut c, "a", Method::Invite);
    let early = Carried::answering(&Method::Invite, 183);
    send(&mut c, "a", Author::Leg("b-2"), early, &sdp("carol 303 4 IN IP4 192.0.2.3", 30002));
    let fin = Carried::answering(&Method::Invite, 200);
    let final_ =
        send(&mut c, "a", Author::Leg("b-2"), fin, &sdp("carol 303 5 IN IP4 192.0.2.3", 30004));
    assert_eq!(o_line(&final_), "o=bob 202 4 IN IP4 192.0.2.2");
}

/// Each new offer of the peer opens a new exchange: an identical answer to it
/// is a new version still.
#[test]
fn an_identical_answer_to_a_new_offer_is_a_new_version() {
    let mut c = spliced();
    let answer = sdp("carol 303 4 IN IP4 192.0.2.3", 30002);
    for version in 3..6 {
        peer_offers(&mut c, "a", Method::Invite);
        let out = send(
            &mut c,
            "a",
            Author::Leg("b-2"),
            Carried::answering(&Method::Invite, 200),
            &answer,
        );
        assert_eq!(o_line(&out), format!("o=bob 202 {version} IN IP4 192.0.2.2"));
    }
}

/// An offerless re-INVITE's reliable 183 carries the offer, and the PRACK
/// carries the peer's answer to it — no offer of the peer's: the 200 repeating
/// the 183's description is the 183's bytes (RFC 6337 §3.1.1).
#[test]
fn a_prack_answer_opens_no_exchange() {
    let mut c = spliced();
    peer_sends(&mut c, "a", "INVITE", false);
    let offer = sdp("carol 303 5 IN IP4 192.0.2.3", 30002);
    let early =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 183), &offer);
    peer_sends(&mut c, "a", "PRACK", true);
    let fin =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
    assert_eq!(fin, early, "the 200 repeats the 183 byte for byte");
}

/// The final to the peer's re-INVITE repeating the description the author
/// already offered in a nested UPDATE is that description again: the stored
/// bytes, the same version (RFC 6337 §3.1).
#[test]
fn a_final_repeating_the_authors_nested_offer_repeats_its_restatement() {
    let mut c = spliced();
    peer_offers(&mut c, "a", Method::Invite);
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::answering(&Method::Invite, 183),
        &sdp("carol 303 4 IN IP4 192.0.2.3", 30002),
    );
    let offer = sdp("carol 303 5 IN IP4 192.0.2.3", 30004);
    let update = send(&mut c, "a", Author::Leg("b-2"), Carried::InDialog, &offer);
    let fin =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
    assert_eq!(fin, update, "the same description, the same version");
}
