//! A route stating `BodyUpdate::AttachParts` mints the callee's INVITE with the
//! caller's session description beside the stated parts (RFC 5621 §3): framed
//! `multipart/mixed` when the caller offered, the one part as the whole body
//! when the caller did not (RFC 3261 §13.2.1 delayed offer), `MIME-Version`
//! on the request either way (RFC 2045 §4).
//! The parts ride byte-exact with their own entity headers, on the initial
//! route and on a failover route alike.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{BodyUpdate, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::{Agent, ClientInvite, Harness};
use sip_message::header::{HeaderName, MediaType, ParamValue};
use sip_message::{decompose_multipart, MultipartPart, SipRequest};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";

/// Binary content: bytes that are not UTF-8, a NUL, and a CRLF followed by
/// `--`, which no boundary may match.
const DATA: &[u8] = &[0x77, 0x12, 0x00, 0x83, 0xFF, 0x0D, 0x0A, 0x2D, 0x2D, 0xFE];
const DATA_CT: &str = "application/vnd.example.data";

fn data_part() -> MultipartPart {
    MultipartPart::new(DATA_CT, DATA.to_vec())
        .with_header("Content-Transfer-Encoding", "binary")
        .with_header("Content-Disposition", "signal;handling=optional")
}

fn location_part() -> MultipartPart {
    MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
        .with_header("Content-ID", "<loc@example.invalid>")
}

fn attaching(parts: Vec<MultipartPart>, port: u16) -> b2bua::decision::RouteDecision {
    let mut r = route_to("127.0.0.1", port);
    r.update_body = BodyUpdate::AttachParts(parts);
    r
}

fn lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The callee's INVITE frames the caller's offer, then [`data_part`].
fn assert_offer_beside_the_data(req: &SipRequest) {
    let ct = req.header::<MediaType>().expect("a Content-Type").expect("it parses");
    assert!(ct.is("multipart/mixed"), "offer and part are framed: {}", ct.token());
    assert_eq!(lines(req, "MIME-Version"), ["1.0"]);
    let boundary = ct.param("boundary").and_then(ParamValue::as_str).expect("a boundary");
    let body = req.body();
    let parts = decompose_multipart(body, boundary);
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert_eq!(parts[0].content_type, "application/sdp", "the description comes first");
    assert_eq!(&body[parts[0].offset..][..parts[0].len], OFFER.as_bytes());
    assert_eq!(parts[1].content_type, DATA_CT);
    assert_eq!(parts[1].headers, data_part().headers, "the part's own entity headers");
    assert_eq!(&body[parts[1].offset..][..parts[1].len], DATA, "the part is byte-exact");
}

/// Answer `uas`'s INVITE, ACK, hang up from the caller, and check the call
/// was answered and fully reaped.
async fn answer_and_hang_up(
    h: Harness,
    b2bua: B2buaSut,
    callee: &Agent,
    mut call: ClientInvite,
    mut uas: scenario_harness::ServerTxn,
) {
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    callee.receive("ACK").await;
    let mut bye = dialog.bye().await;
    callee.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let kinds: Vec<CdrEventType> =
        b2bua.cdr_records()[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Answer), "answered: {kinds:?}");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test]
async fn an_offer_and_an_attached_part_frame_as_multipart_mixed() {
    let h = Harness::with_transit_delay("b2bua-attached-parts-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(attaching(vec![data_part()], 5074)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5084").await;

    let call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let uas = bob.receive("INVITE").await;
    assert_offer_beside_the_data(uas.request());
    answer_and_hang_up(h, b2bua, &bob, call, uas).await;
}

/// Without an offer the one part is the body, its entity headers and
/// `MIME-Version` on the request; the callee's 200 carries the offer and the caller's ACK the
/// answer, relayed (RFC 3264 §4).
#[tokio::test]
async fn without_an_offer_the_one_part_is_the_body() {
    let h = Harness::with_transit_delay("b2bua-attached-parts-delayed-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(attaching(vec![location_part()], 5075)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5085").await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let req = uas.request();
    let ct = req.header::<MediaType>().expect("a Content-Type").expect("it parses");
    assert!(ct.is("application/pidf+xml"), "{}", ct.token());
    assert_eq!(lines(req, "Content-ID"), ["<loc@example.invalid>"]);
    assert_eq!(lines(req, "MIME-Version"), ["1.0"], "the part is a MIME entity");
    assert_eq!(req.body().as_ref(), location_part().payload.as_slice());

    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().body().as_ref(), ANSWER.as_bytes(), "the caller's answer");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A failover route attaches its parts to the next leg's INVITE as the
/// initial route does.
#[tokio::test]
async fn a_failover_route_attaches_its_parts() {
    let h = Harness::with_transit_delay("b2bua-attached-parts-failover", 0);
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let carol = h.agent("carol", "127.0.0.1:5077").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5076);
                r.callback_context = Some("ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(attaching(vec![data_part()], 5077)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5086").await;

    let call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut first = bob.receive("INVITE").await;
    assert_eq!(first.request().body().as_ref(), OFFER.as_bytes(), "the first route keeps");
    first.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let uas = carol.receive("INVITE").await;
    assert_offer_beside_the_data(uas.request());
    answer_and_hang_up(h, b2bua, &carol, call, uas).await;
}
