//! The seam's decisions on sequences of descriptions crossing one dialog,
//! each named by the leg its author spoke on.

use call::{Call, LegState};
use sip_message::header::MediaType;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipStr};

use super::{adopt_confirmed_dialog, continue_on_leg, next_origin_in_dialog, Author, Carried};
use crate::config::{AsWritten, B2buaConfig, SdpCrossing, SdpForm, SdpFormPolicy};
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

/// A service policy that re-serializes a peer's description once the call has
/// a restatement: the test stand-in for a deployment's rule.
#[derive(Debug)]
struct CanonicalOnceRestated;

impl SdpFormPolicy for CanonicalOnceRestated {
    fn form(&self, crossing: &SdpCrossing) -> SdpForm {
        if crossing.by_peer && crossing.call_restated {
            SdpForm::Canonical
        } else {
            SdpForm::AsWritten
        }
    }
}

/// `AsWritten` is the stack's default; `Canonical` names [`CanonicalOnceRestated`].
fn policy(form: SdpForm) -> &'static dyn SdpFormPolicy {
    match form {
        SdpForm::AsWritten => &AsWritten,
        SdpForm::Canonical => &CanonicalOnceRestated,
    }
}

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
    send_in(call, to, author, carried, body, SdpForm::AsWritten)
}

/// [`send`] with the stack's form after a restatement stated.
fn send_in(
    call: &mut Call,
    to: &str,
    author: Author<'_>,
    carried: Carried,
    body: &str,
    form: SdpForm,
) -> String {
    let out = continue_on_leg(
        call,
        to,
        None,
        author,
        carried,
        body.as_bytes().to_vec(),
        Some(&ct()),
        policy(form),
    );
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

/// The peer of `leg` sends a `method` request this stack relays: received as
/// [`peer_sends`], then pending on the relay target's dialog until its final.
fn peer_relays(call: &mut Call, leg: &str, method: &str, with_sdp: bool) {
    peer_sends(call, leg, method, with_sdp);
    let tags = call::helpers::RequestTags::new(Some("b"), None);
    let target = call::helpers::resolve_relay_peer(call, leg, tags).0.expect("a relay target");
    let pending = call::PendingRequest {
        method: method.into(),
        outbound_cseq: 7,
        inbound_cseq: 7,
        source_vias: vec![format!("SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-{method}")],
        source_call_id: "c".into(),
        source_from: "<sip:a@192.0.2.1>;tag=a".into(),
        source_to: "<sip:b@192.0.2.100>;tag=b".into(),
        direction: call::Direction::FromA,
        cancelled: false,
        offered_100rel: true,
        offered: with_sdp,
        source_timestamp: None,
    };
    let target = std::iter::once(&mut call.a_leg)
        .chain(call.b_legs.iter_mut())
        .find(|l| l.leg_id == target)
        .unwrap();
    if target.dialogs.is_empty() {
        let ctx = call::helpers::MakeDialogLegCtx {
            call_id: "c-b",
            local_uri: "sip:b2bua@192.0.2.100",
            remote_uri: "sip:bob@192.0.2.2",
            local_tag: "as",
            remote_tag: "bob",
        };
        target.dialogs.push(call::helpers::make_empty_dialog(&ctx, 1));
    }
    target.dialogs[0].ext.inbound_pending_requests.push(pending);
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

/// Each offerless re-INVITE of the peer opens a new exchange whose offer is
/// the final (RFC 3264 §5): the author's unchanged description in it is a new
/// version each time.
#[test]
fn an_unchanged_offer_in_the_final_to_an_offerless_invite_is_a_new_version() {
    let mut c = spliced();
    let offer = sdp("carol 303 4 IN IP4 192.0.2.3", 30002);
    for version in 3..6 {
        peer_sends(&mut c, "a", "INVITE", false);
        let out =
            send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
        assert_eq!(o_line(&out), format!("o=bob 202 {version} IN IP4 192.0.2.2"));
        let answer = sdp("alice 101 2 IN IP4 192.0.2.1", 10002);
        send(&mut c, "b-2", Author::Leg("a"), Carried::InDialog, &answer);
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

/// A second INVITE of the peer while its first is still pending is refused
/// (RFC 3261 §14.1) and opens no exchange: the 200 repeating the reliable
/// 183's offer is the 183's bytes (RFC 6337 §3.1.1).
#[test]
fn an_invite_refused_inside_the_peers_open_invite_opens_no_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "INVITE", false);
    let offer = sdp("carol 303 5 IN IP4 192.0.2.3", 30002);
    let early =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 183), &offer);
    peer_sends(&mut c, "a", "INVITE", false);
    peer_sends(&mut c, "a", "PRACK", true);
    let fin =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
    assert_eq!(fin, early, "the 200 repeats the 183 byte for byte");
}

/// The compliant nested exchange (RFC 3311 §5.1): once the reliable 183's
/// offer is answered in the PRACK, the peer's UPDATE offer inside its pending
/// INVITE opens its own exchange, so the author's unchanged answer is a new
/// version; the INVITE's 200 repeating that answer is the UPDATE 200's bytes.
#[test]
fn an_update_after_the_prack_inside_the_peers_open_invite_opens_an_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "INVITE", false);
    let offer = sdp("carol 303 5 IN IP4 192.0.2.3", 30002);
    let early =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 183), &offer);
    assert_eq!(o_line(&early), "o=bob 202 3 IN IP4 192.0.2.2");
    peer_sends(&mut c, "a", "PRACK", true);
    peer_relays(&mut c, "a", "UPDATE", true);
    let update =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Update, 200), &offer);
    assert_eq!(o_line(&update), "o=bob 202 4 IN IP4 192.0.2.2", "a new exchange");
    let fin =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
    assert_eq!(fin, update, "the 200 repeats the UPDATE's answer");
}

/// An UPDATE offer of the peer while the reliable 183's offer awaits its PRACK
/// breaks RFC 3311 §5.1. The call does not record whether that 183's
/// description is an offer, so the UPDATE opens an exchange: the 200
/// repeating the 183's description is a new version, which RFC 3264 §8
/// permits.
#[test]
fn an_update_before_the_prack_opens_an_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "INVITE", false);
    let offer = sdp("carol 303 5 IN IP4 192.0.2.3", 30002);
    send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 183), &offer);
    peer_relays(&mut c, "a", "UPDATE", true);
    peer_sends(&mut c, "a", "PRACK", true);
    let fin =
        send(&mut c, "a", Author::Leg("b-2"), Carried::answering(&Method::Invite, 200), &offer);
    assert_eq!(o_line(&fin), "o=bob 202 4 IN IP4 192.0.2.2");
}

/// An UPDATE offer of the peer inside its own pending INVITE whose offer no
/// reliable provisional answered overlaps that offer (RFC 3311 §5.2) and opens
/// no exchange, whether or not the INVITE offered `100rel`.
#[test]
fn an_update_inside_the_peers_open_offer_invite_opens_no_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "INVITE", true);
    let opened = c.a_leg.sdp_session.exchanges_opened;
    peer_sends(&mut c, "a", "UPDATE", true);
    assert_eq!(c.a_leg.sdp_session.exchanges_opened, opened);
}

/// Once a reliable provisional shown on the peer's face carried the answer to
/// its INVITE's offer, its UPDATE offer is the legal nesting of RFC 3311 §5.1
/// and opens an exchange.
#[test]
fn an_update_after_a_reliable_answer_to_the_peers_invite_opens_an_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "INVITE", true);
    c.reliable_provisionals.push(call::ReliableProvisional {
        a_tag: "b".into(),
        a_rseq: 1,
        b_leg_id: "b-2".into(),
        b_tag: "carol".into(),
        b_cseq: 7,
        b_rseq: 1,
        acknowledged: true,
        emission: None,
        a_cseq: 7,
        carried_sdp: true,
        responder_sdp: true,
        responder_offer: None,
    });
    let opened = c.a_leg.sdp_session.exchanges_opened;
    peer_sends(&mut c, "a", "UPDATE", true);
    assert_eq!(c.a_leg.sdp_session.exchanges_opened, opened + 1);
}

/// A second UPDATE offer of the peer while its first is pending (RFC 3311
/// §5.2) opens no exchange.
#[test]
fn an_update_inside_the_peers_open_update_opens_no_exchange() {
    let mut c = spliced();
    peer_relays(&mut c, "a", "UPDATE", true);
    let opened = c.a_leg.sdp_session.exchanges_opened;
    peer_sends(&mut c, "a", "UPDATE", true);
    assert_eq!(c.a_leg.sdp_session.exchanges_opened, opened);
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

/// Leg `b-1` unconfirmed, opened by the caller's INVITE (`alice 1 1`), with
/// the early dialogs `f1` and `f2`.
fn forked() -> Call {
    let mut c = call();
    let leg = &mut c.b_legs[0];
    leg.state = LegState::Early;
    leg.dialogs = ["f1", "f2"]
        .into_iter()
        .map(|tag| {
            let ctx = call::helpers::MakeDialogLegCtx {
                call_id: "b-1",
                local_uri: "sip:alice@192.0.2.1",
                remote_uri: "sip:bob@192.0.2.2",
                local_tag: "as",
                remote_tag: tag,
            };
            call::helpers::make_empty_dialog(&ctx, 1)
        })
        .collect();
    send(
        &mut c,
        "b-1",
        Author::Leg("a"),
        Carried::Opening,
        &sdp("alice 1 1 IN IP4 192.0.2.1", 10000),
    );
    c
}

/// The stack's own answer inside `f1` of `b-1`, stating `origin`.
fn answer_in_f1(c: &mut Call, origin: &str) -> String {
    let body = sdp(origin, 10000);
    let out = continue_on_leg(
        c,
        "b-1",
        Some("f1"),
        Author::Stack,
        Carried::InDialog,
        body.as_bytes().to_vec(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    String::from_utf8(out).unwrap()
}

/// A description of the stack's own that already states the next version of
/// the dialog's session leaves as written, and the author's next description
/// is restated above it, early dialog or not.
#[test]
fn a_stack_description_stating_the_next_version_leaves_as_written() {
    let mut c = forked();
    let answer = answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    assert_eq!(answer, sdp("alice 1 2 IN IP4 192.0.2.1", 10000), "not restated a second time");
    assert_eq!(
        answer_in_f1(&mut c, "alice 1 3 IN IP4 192.0.2.1"),
        sdp("alice 1 3 IN IP4 192.0.2.1", 10000)
    );
    let relayed = continue_on_leg(
        &mut c,
        "b-1",
        Some("f1"),
        Author::Leg("a"),
        Carried::InDialog,
        sdp("alice 1 2 IN IP4 192.0.2.1", 10002).into_bytes(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    assert_eq!(o_line(&String::from_utf8(relayed).unwrap()), "o=alice 1 4 IN IP4 192.0.2.1");
}

/// What the stack states inside one early dialog is that dialog's: the leg and
/// the other early dialogs keep the opening state, and the dialog that
/// confirms hands its own to the leg.
#[test]
fn each_early_dialog_keeps_its_own_session() {
    let mut c = forked();
    answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    let leg = &c.b_legs[0];
    assert_eq!(leg.sdp_session.sent_origin.as_deref(), Some("alice 1 1 IN IP4 192.0.2.1"));
    assert_eq!(next_origin_in_dialog(leg, "f1").as_deref(), Some("alice 1 3 IN IP4 192.0.2.1"));
    assert_eq!(next_origin_in_dialog(leg, "f2").as_deref(), Some("alice 1 2 IN IP4 192.0.2.1"));

    let mut other = c.clone();
    adopt_confirmed_dialog(&mut other.b_legs[0], 1);
    other.b_legs[0].state = LegState::Confirmed;
    let reoffer = sdp("alice 1 2 IN IP4 192.0.2.1", 10002);
    assert_eq!(
        send(&mut other, "b-1", Author::Leg("a"), Carried::InDialog, &reoffer),
        reoffer,
        "f2 never saw a version of the stack's: the caller's own continues",
    );

    adopt_confirmed_dialog(&mut c.b_legs[0], 0);
    c.b_legs[0].state = LegState::Confirmed;
    let restated = send(&mut c, "b-1", Author::Leg("a"), Carried::InDialog, &reoffer);
    assert_eq!(
        o_line(&restated),
        "o=alice 1 3 IN IP4 192.0.2.1",
        "above the stack's version in f1"
    );
}

/// A relayed early-dialog exchange keeps the dialog's state current: the
/// caller's bodiless PRACK and her 200 answering the callee's UPDATE both
/// cross `f1`, and `f1` confirming leaves the leg on what she last stated.
#[test]
fn a_relayed_early_exchange_survives_confirmation() {
    let mut c = forked();
    continue_on_leg(
        &mut c,
        "b-1",
        Some("f1"),
        Author::Leg("a"),
        Carried::InDialog,
        vec![],
        None,
        policy(SdpForm::AsWritten),
    );
    let answer = sdp("alice 1 2 IN IP4 192.0.2.1", 10002);
    let out = continue_on_leg(
        &mut c,
        "b-1",
        Some("f1"),
        Author::Leg("a"),
        Carried::answering(&Method::Update, 200),
        answer.clone().into_bytes(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    assert_eq!(String::from_utf8(out).unwrap(), answer);
    adopt_confirmed_dialog(&mut c.b_legs[0], 0);
    assert_eq!(
        c.b_legs[0].sdp_session.sent_origin.as_deref(),
        Some("alice 1 2 IN IP4 192.0.2.1"),
        "the leg holds what the caller last stated in f1",
    );
}

/// What crosses one early dialog stays that dialog's: the caller's answer
/// relayed into `f1` is not what `f2` confirms with.
#[test]
fn a_relayed_description_does_not_leak_into_another_fork() {
    let mut c = forked();
    continue_on_leg(
        &mut c,
        "b-1",
        Some("f1"),
        Author::Leg("a"),
        Carried::answering(&Method::Update, 200),
        sdp("alice 1 2 IN IP4 192.0.2.1", 10002).into_bytes(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    adopt_confirmed_dialog(&mut c.b_legs[0], 1);
    assert_eq!(
        c.b_legs[0].sdp_session.sent_origin.as_deref(),
        Some("alice 1 1 IN IP4 192.0.2.1"),
        "f2 only ever saw the opening INVITE",
    );
}

/// On a confirmed dialog a stack description is restated whatever version it
/// states: one at the next version but with fewer streams than the dialog
/// holds keeps the missing slot rejected in place (RFC 3264 §8), instead of
/// leaving as written.
#[test]
fn a_confirmed_dialog_restates_a_stack_description_at_the_next_version() {
    let mut c = call();
    let two = "v=0\r\no=alice 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nm=video 10002 RTP/AVP 96\r\n";
    send(&mut c, "b-1", Author::Leg("a"), Carried::Opening, two);
    let out = send(
        &mut c,
        "b-1",
        Author::Stack,
        Carried::InDialog,
        &sdp("alice 1 2 IN IP4 192.0.2.1", 10000),
    );
    assert_eq!(o_line(&out), "o=alice 1 2 IN IP4 192.0.2.1");
    assert!(out.ends_with("m=audio 10000 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n"), "{out}");
}

// ── the form a description leaves in once the call has a restatement ──

/// A description as a peer writes it: the `fmtp` before the `rtpmap` lines,
/// no direction attribute.
fn peer_sdp(origin: &str, port: u16) -> String {
    format!(
        "v=0\r\no={origin}\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 8 18\r\na=fmtp:18 annexb=no\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=ptime:20\r\n"
    )
}

/// [`peer_sdp`] in canonical form.
fn in_form(origin: &str, port: u16) -> String {
    format!(
        "v=0\r\no={origin}\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 8 18\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=no\r\na=ptime:20\r\na=sendrecv\r\n"
    )
}

/// Caller `a` answered by `b-1`, the transfer target `b-2` offering: the
/// first restatement of the call, toward the caller.
fn restated_once(form: SdpForm) -> (Call, String) {
    let mut c = call();
    send_in(
        &mut c,
        "a",
        Author::Leg("b-1"),
        Carried::Opening,
        &peer_sdp("bob 202 1 IN IP4 192.0.2.2", 20000),
        form,
    );
    let out = send_in(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("carol 303 1 IN IP4 192.0.2.3", 30000),
        form,
    );
    (c, out)
}

/// Until the call's first restatement a peer's description leaves as its
/// author wrote it, whatever the form: a plain relay is byte-transparent.
#[test]
fn before_any_restatement_a_description_leaves_as_written() {
    let mut c = call();
    for (carried, version) in [(Carried::Opening, 1), (Carried::InDialog, 2)] {
        let body = peer_sdp(&format!("bob 202 {version} IN IP4 192.0.2.2"), 20000);
        let out = send_in(&mut c, "a", Author::Leg("b-1"), carried, &body, SdpForm::Canonical);
        assert_eq!(out, body);
    }
}

/// The restatement itself leaves in canonical form; under `AsWritten` it
/// keeps the author's attribute order.
#[test]
fn a_restatement_leaves_in_canonical_form() {
    let (_, out) = restated_once(SdpForm::Canonical);
    assert_eq!(out, in_form("bob 202 2 IN IP4 192.0.2.2", 30000));
    let (_, out) = restated_once(SdpForm::AsWritten);
    assert_eq!(out, peer_sdp("bob 202 2 IN IP4 192.0.2.2", 30000));
}

/// Once the call has a restatement, a peer's description leaves in canonical
/// form on every leg, restated or not: the caller's answer opening the
/// transfer target's dialog, and the target's own next description relayed
/// back as its author's.
#[test]
fn after_a_restatement_every_peer_description_leaves_in_canonical_form() {
    let (mut c, _) = restated_once(SdpForm::Canonical);
    let answer = send_in(
        &mut c,
        "b-3",
        Author::Leg("a"),
        Carried::InDialog,
        &peer_sdp("alice 101 1 IN IP4 192.0.2.1", 10000),
        SdpForm::Canonical,
    );
    assert_eq!(answer, in_form("alice 101 1 IN IP4 192.0.2.1", 10000), "not restated, in form");
    let again = send_in(
        &mut c,
        "b-3",
        Author::Leg("a"),
        Carried::InDialog,
        &peer_sdp("alice 101 2 IN IP4 192.0.2.1", 10002),
        SdpForm::Canonical,
    );
    assert_eq!(again, in_form("alice 101 2 IN IP4 192.0.2.1", 10002));
}

/// The stack's own description leaves as the stack wrote it, restatement or
/// not: only a peer's description is re-serialized.
#[test]
fn a_stack_description_keeps_its_own_form_after_a_restatement() {
    let (mut c, _) = restated_once(SdpForm::Canonical);
    let own = peer_sdp("as 7 7 IN IP4 192.0.2.9", 40000);
    let out = send_in(&mut c, "b-3", Author::Stack, Carried::Opening, &own, SdpForm::Canonical);
    assert_eq!(out, own);
}

/// A description of the stack's own stating the next version inside an early
/// dialog restates nothing: a peer's description elsewhere in the call still
/// leaves as written.
#[test]
fn a_stack_description_at_the_next_version_is_no_restatement() {
    let mut c = forked();
    answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::Opening,
        &peer_sdp("dave 4 1 IN IP4 192.0.2.4", 40000),
    );
    let body = peer_sdp("dave 4 2 IN IP4 192.0.2.4", 40002);
    let out =
        send_in(&mut c, "a", Author::Leg("b-2"), Carried::InDialog, &body, SdpForm::Canonical);
    assert_eq!(out, body);
}

/// The restatement is the call's: a leg that restated nothing itself, whose
/// state travels with a replicated or reclaimed call, still sees it.
#[test]
fn the_calls_restatement_is_recorded_on_the_leg_it_crossed() {
    let (c, _) = restated_once(SdpForm::AsWritten);
    assert!(c.a_leg.sdp_session.has_restated, "the caller's leg carried a restatement");
    assert!(c.b_legs.iter().all(|l| !l.sdp_session.has_restated));
}

/// A restatement inside an early dialog whose fork then loses stays the
/// call's: the confirming fork and the pruned forks do not erase it.
#[test]
fn a_restatement_in_a_losing_fork_stays_the_calls() {
    let mut c = forked();
    answer_in_f1(&mut c, "alice 1 2 IN IP4 192.0.2.1");
    continue_on_leg(
        &mut c,
        "b-1",
        Some("f1"),
        Author::Leg("a"),
        Carried::InDialog,
        sdp("alice 1 2 IN IP4 192.0.2.1", 10002).into_bytes(),
        Some(&ct()),
        policy(SdpForm::AsWritten),
    );
    adopt_confirmed_dialog(&mut c.b_legs[0], 1);
    let winner = c.b_legs[0].dialogs.remove(1);
    c.b_legs[0].dialogs = vec![winner];
    c.b_legs[0].state = LegState::Confirmed;
    assert!(c.b_legs[0].sdp_session.has_restated, "the confirmed leg keeps the call's fact");
    send(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::Opening,
        &peer_sdp("dave 4 1 IN IP4 192.0.2.4", 40000),
    );
    let out = send_in(
        &mut c,
        "a",
        Author::Leg("b-2"),
        Carried::InDialog,
        &peer_sdp("dave 4 2 IN IP4 192.0.2.4", 40002),
        SdpForm::Canonical,
    );
    assert_eq!(out, in_form("dave 4 2 IN IP4 192.0.2.4", 40002));
}

/// The leg's own state, set aside while a description crosses one of its early
/// dialogs, still states the call's restatement.
#[test]
fn the_calls_restatement_is_read_through_an_early_dialog() {
    let mut c = forked();
    let relay_in_f1 = |c: &mut Call, version: u32, form: SdpForm| {
        let out = continue_on_leg(
            c,
            "b-1",
            Some("f1"),
            Author::Leg("a"),
            Carried::answering(&Method::Update, 200),
            peer_sdp(&format!("alice 1 {version} IN IP4 192.0.2.1"), 10002).into_bytes(),
            Some(&ct()),
            policy(form),
        );
        String::from_utf8(out).unwrap()
    };
    relay_in_f1(&mut c, 2, SdpForm::AsWritten);
    c.b_legs[0].sdp_session.has_restated = true;
    assert_eq!(
        relay_in_f1(&mut c, 3, SdpForm::Canonical),
        in_form("alice 1 3 IN IP4 192.0.2.1", 10002)
    );
}

/// Every crossing the seam asks a policy about, in order.
#[derive(Debug, Default)]
struct Recording(std::sync::Mutex<Vec<SdpCrossing>>);

impl SdpFormPolicy for Recording {
    fn form(&self, crossing: &SdpCrossing) -> SdpForm {
        self.0.lock().unwrap().push(*crossing);
        SdpForm::AsWritten
    }
}

/// The seam asks the policy about in-dialog descriptions only, and states
/// who wrote each and whether the call, or this description, is restated.
#[test]
fn the_policy_sees_each_in_dialog_crossing_as_it_stands() {
    let rec = Recording::default();
    let mut c = call();
    let cross = |c: &mut Call, author: Author<'_>, carried: Carried, body: String| {
        continue_on_leg(c, "a", None, author, carried, body.into_bytes(), Some(&ct()), &rec);
    };
    cross(&mut c, Author::Leg("b-1"), Carried::Opening, peer_sdp("bob 202 1 IN IP4 192.0.2.2", 1));
    cross(&mut c, Author::Leg("b-1"), Carried::InDialog, peer_sdp("bob 202 2 IN IP4 192.0.2.2", 2));
    cross(&mut c, Author::Leg("b-2"), Carried::InDialog, peer_sdp("carol 3 1 IN IP4 192.0.2.3", 3));
    cross(&mut c, Author::Stack, Carried::InDialog, peer_sdp("as 7 7 IN IP4 192.0.2.9", 4));
    let seen = rec.0.lock().unwrap().clone();
    let crossing =
        |by_peer, restated, call_restated| SdpCrossing { by_peer, restated, call_restated };
    assert_eq!(
        seen,
        vec![crossing(true, false, false), crossing(true, true, true), crossing(false, true, true)],
        "the opening description is not asked about"
    );
}
