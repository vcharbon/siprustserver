//! The synthetic seeds the transaction-layer lane mutates: requests a peer
//! sends the layer, and responses the peer sends to the two client
//! transactions the lane opens ([`client_requests`]). A seed also names the
//! unmutated datagrams sent before its mutated copy (the INVITE a CANCEL or an
//! ACK belongs to) and after it (the CANCEL of a mutated INVITE). Addresses
//! are documentation ranges (RFC 5737) and `example.*` names.

use std::time::Duration;

use call::TxnKind;
use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};

use super::drive::FINAL_DELAY;

/// Where the layer under test listens.
pub const LAYER: &str = "192.0.2.10:5060";
/// Where the peer the datagrams come from listens.
pub const PEER: &str = "192.0.2.20:5060";

/// The call the lane's client INVITEs are attributed to (their Via `cr`).
pub const INVITE_CALL_REF: &str = "call-1";

/// One seed: a stable name, the bytes the lane mutates, and the unmutated
/// datagrams around them.
pub struct Seed {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub before: Vec<Vec<u8>>,
    /// Whether the lane releases the client INVITEs' call after `before`, so
    /// the mutated copy meets an orphan (`cancel_txns_for_call`).
    pub release: bool,
    /// How long the case waits after `before`, so the mutated copy meets
    /// the state it belongs to (the ACK of a 2xx after the 2xx).
    pub settle: Duration,
    pub after: Vec<Vec<u8>>,
}

/// A message from its start line, header lines and body, the Content-Length
/// computed and appended last.
fn message(start: &str, headers: &[&str], body: &str) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(start);
    out.push_str("\r\n");
    for h in headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    out.into_bytes()
}

const OFFER: &str = "v=0\r\no=alice 2890844526 2890844526 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0 8 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 2808844564 2808844564 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 5004 RTP/AVP 8 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n";

const PEER_VIA: &str = "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-peer-1;rport";
const PEER_FROM: &str = "From: \"Alice\" <sip:alice@example.com>;tag=a1";
const PEER_TO: &str = "To: <sip:bob@example.net>";
const PEER_TO_TAGGED: &str = "To: <sip:bob@example.net>;tag=b2";
const PEER_CALL_ID: &str = "Call-ID: lane-call-1@192.0.2.20";

/// The initial INVITE every INVITE-shaped request seed belongs to.
fn invite() -> Vec<u8> {
    message(
        "INVITE sip:bob@192.0.2.10:5060 SIP/2.0",
        &[
            PEER_VIA,
            "Via: SIP/2.0/UDP 198.51.100.7:5070;branch=z9hG4bK-up-1;received=198.51.100.7",
            "Max-Forwards: 70",
            PEER_FROM,
            PEER_TO,
            PEER_CALL_ID,
            "CSeq: 1 INVITE",
            "Contact: <sip:alice@192.0.2.20:5060;transport=udp>;expires=3600",
            "Record-Route: <sip:192.0.2.30;lr>, <sip:rr.example.com;lr>",
            "Supported: 100rel, timer",
            "Session-Expires: 1800;refresher=uac",
            "Min-SE: 90",
            "Allow: INVITE, ACK, CANCEL, BYE, PRACK, UPDATE, INFO",
            "Resource-Priority: esnet.0",
            "Content-Type: application/sdp",
        ],
        OFFER,
    )
}

/// The CANCEL of [`invite`].
fn cancel() -> Vec<u8> {
    message(
        "CANCEL sip:bob@192.0.2.10:5060 SIP/2.0",
        &[
            PEER_VIA,
            "Max-Forwards: 70",
            PEER_FROM,
            PEER_TO,
            PEER_CALL_ID,
            "CSeq: 1 CANCEL",
            "Reason: SIP;cause=200;text=\"Call completed elsewhere\"",
        ],
        "",
    )
}

/// The ACK of the TU's 2xx to [`invite`], on its own branch (RFC 3261
/// §13.2.2.4).
fn ack() -> Vec<u8> {
    message(
        "ACK sip:bob@192.0.2.10:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-peer-ack;rport",
            "Max-Forwards: 70",
            PEER_FROM,
            "To: <sip:bob@example.net>;tag=lane-uas",
            PEER_CALL_ID,
            "CSeq: 1 ACK",
        ],
        "",
    )
}

/// An in-dialog request of `method` with CSeq number `seq` on its own branch.
fn in_dialog(method: &str, seq: u32, extra: &[&str], body: &str) -> Vec<u8> {
    let via = format!("Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-peer-{seq};rport");
    let cseq = format!("CSeq: {seq} {method}");
    let mut headers = vec![
        via.as_str(),
        "Max-Forwards: 70",
        PEER_FROM,
        PEER_TO_TAGGED,
        PEER_CALL_ID,
        cseq.as_str(),
        "Route: <sip:192.0.2.10:5060;lr>",
        "Contact: <sip:alice@192.0.2.20:5060>",
    ];
    headers.extend_from_slice(extra);
    message(&format!("{method} sip:bob@192.0.2.10:5060 SIP/2.0"), &headers, body)
}

/// An out-of-dialog request of `method`.
fn out_of_dialog(method: &str, extra: &[&str], body: &str) -> Vec<u8> {
    let cseq = format!("CSeq: 7 {method}");
    let mut headers = vec![
        "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-peer-ood;rport",
        "Max-Forwards: 70",
        PEER_FROM,
        PEER_TO,
        "Call-ID: lane-ood-1@192.0.2.20",
        cseq.as_str(),
    ];
    headers.extend_from_slice(extra);
    message(&format!("{method} sip:bob@192.0.2.10:5060 SIP/2.0"), &headers, body)
}

const CLIENT_FROM: &str = "From: <sip:lane@192.0.2.10:5060>;tag=l1";
const CLIENT_CALL_ID: &str = "Call-ID: lane-client-1@192.0.2.10";

/// The requests the lane sends the peer before each case: an initial INVITE,
/// one carrying no offer (RFC 3261 §13.2.1) and a BYE, each a client
/// transaction the response seeds answer.
pub fn client_requests() -> Vec<(SipRequest, TxnKind)> {
    let invite = message(
        "INVITE sip:carol@192.0.2.20:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-inv;cr=call-1;lg=b;ci=k3x9",
            "Max-Forwards: 70",
            CLIENT_FROM,
            "To: <sip:carol@example.net>",
            CLIENT_CALL_ID,
            "CSeq: 1 INVITE",
            "Contact: <sip:lane@192.0.2.10:5060>",
            "Supported: 100rel",
            "Content-Type: application/sdp",
        ],
        OFFER,
    );
    let offerless = message(
        "INVITE sip:erin@192.0.2.20:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-dly;cr=call-1;lg=c;ci=k3x9",
            "Max-Forwards: 70",
            CLIENT_FROM,
            "To: <sip:erin@example.net>",
            "Call-ID: lane-client-3@192.0.2.10",
            "CSeq: 1 INVITE",
            "Contact: <sip:lane@192.0.2.10:5060>",
        ],
        "",
    );
    let bye = message(
        "BYE sip:dave@192.0.2.20:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-bye;cr=call-2;lg=a",
            "Max-Forwards: 70",
            CLIENT_FROM,
            "To: <sip:dave@example.net>;tag=d4",
            "Call-ID: lane-client-2@192.0.2.10",
            "CSeq: 2 BYE",
        ],
        "",
    );
    vec![
        (request_of(&invite), TxnKind::Invite),
        (request_of(&offerless), TxnKind::Invite),
        (request_of(&bye), TxnKind::NonInvite),
    ]
}

fn request_of(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("a lane client request reads as a request: {other:?}"),
    }
}

/// A response the peer sends to the client INVITE.
fn invite_response(status: &str, extra: &[&str], body: &str) -> Vec<u8> {
    let mut headers = vec![
        "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-inv;cr=call-1;lg=b;ci=k3x9;received=192.0.2.10",
        CLIENT_FROM,
        "To: <sip:carol@example.net>;tag=c3",
        CLIENT_CALL_ID,
        "CSeq: 1 INVITE",
    ];
    headers.extend_from_slice(extra);
    message(&format!("SIP/2.0 {status}"), &headers, body)
}

/// The 2xx the peer sends the client INVITE, under the To-tag `tag`.
fn invite_2xx(tag: &str) -> Vec<u8> {
    let to = format!("To: <sip:carol@example.net>;tag={tag}");
    message(
        "SIP/2.0 200 OK",
        &[
            "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-inv;cr=call-1;lg=b;ci=k3x9;received=192.0.2.10",
            CLIENT_FROM,
            to.as_str(),
            CLIENT_CALL_ID,
            "CSeq: 1 INVITE",
            "Contact: <sip:carol@192.0.2.20:5060>",
            "Record-Route: <sip:192.0.2.30;lr>, <sip:rr.example.com;lr>",
            "Content-Type: application/sdp",
        ],
        ANSWER,
    )
}

/// The 2xx the peer sends the offerless client INVITE, carrying the offer
/// (RFC 3261 §13.2.1).
fn delayed_offer_2xx() -> Vec<u8> {
    message(
        "SIP/2.0 200 OK",
        &[
            "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-dly;cr=call-1;lg=c;ci=k3x9",
            CLIENT_FROM,
            "To: <sip:erin@example.net>;tag=e5",
            "Call-ID: lane-client-3@192.0.2.10",
            "CSeq: 1 INVITE",
            "Contact: <sip:erin@192.0.2.20:5060>",
            "Content-Type: application/sdp",
        ],
        OFFER,
    )
}

/// A response the peer sends to the client BYE.
fn bye_response(status: &str, extra: &[&str]) -> Vec<u8> {
    let mut headers = vec![
        "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-lane-bye;cr=call-2;lg=a",
        CLIENT_FROM,
        "To: <sip:dave@example.net>;tag=d4",
        "Call-ID: lane-client-2@192.0.2.10",
        "CSeq: 2 BYE",
    ];
    headers.extend_from_slice(extra);
    message(&format!("SIP/2.0 {status}"), &headers, "")
}

fn seed(name: &'static str, bytes: Vec<u8>) -> Seed {
    Seed {
        name,
        bytes,
        before: Vec::new(),
        release: false,
        settle: Duration::ZERO,
        after: Vec::new(),
    }
}

/// Every seed of the lane.
pub fn all() -> Vec<Seed> {
    vec![
        Seed { after: vec![cancel()], ..seed("req/invite", invite()) },
        Seed { before: vec![invite()], ..seed("req/cancel", cancel()) },
        Seed { before: vec![invite()], settle: FINAL_DELAY, ..seed("req/ack", ack()) },
        seed(
            "req/reinvite",
            in_dialog(
                "INVITE",
                2,
                &["Session-Expires: 90;refresher=uas", "Content-Type: application/sdp"],
                OFFER,
            ),
        ),
        seed("req/bye", in_dialog("BYE", 3, &["Reason: Q.850;cause=16"], "")),
        Seed {
            before: vec![invite()],
            ..seed("req/prack", in_dialog("PRACK", 4, &["RAck: 1 1 INVITE"], ""))
        },
        seed("req/update", in_dialog("UPDATE", 5, &["Content-Type: application/sdp"], OFFER)),
        seed(
            "req/info",
            in_dialog("INFO", 6, &["Content-Type: application/dtmf-relay"], "Signal=5\r\nDuration=160\r\n"),
        ),
        seed(
            "req/refer",
            in_dialog(
                "REFER",
                7,
                &["Refer-To: <sip:carol@example.net?Replaces=abc%40192.0.2.9%3Bto-tag%3D1%3Bfrom-tag%3D2>", "Referred-By: <sip:alice@example.com>"],
                "",
            ),
        ),
        seed(
            "req/notify",
            in_dialog(
                "NOTIFY",
                8,
                &["Event: refer;id=7", "Subscription-State: terminated;reason=noresource", "Content-Type: message/sipfrag"],
                "SIP/2.0 200 OK\r\n",
            ),
        ),
        seed("req/options", out_of_dialog("OPTIONS", &["Accept: application/sdp"], "")),
        seed(
            "req/register",
            out_of_dialog("REGISTER", &["Contact: <sip:alice@192.0.2.20:5060>;expires=600", "Expires: 600"], ""),
        ),
        seed(
            "req/message",
            out_of_dialog("MESSAGE", &["Content-Type: text/plain"], "hello"),
        ),
        seed(
            "req/subscribe",
            out_of_dialog("SUBSCRIBE", &["Event: presence", "Expires: 3600", "Contact: <sip:alice@192.0.2.20>"], ""),
        ),
        seed("resp/100-invite", invite_response("100 Trying", &[], "")),
        seed(
            "resp/180-invite",
            invite_response("180 Ringing", &["Contact: <sip:carol@192.0.2.20:5060>"], ""),
        ),
        seed(
            "resp/183-invite",
            invite_response(
                "183 Session Progress",
                &["Require: 100rel", "RSeq: 1", "Contact: <sip:carol@192.0.2.20:5060>", "Content-Type: application/sdp"],
                ANSWER,
            ),
        ),
        seed(
            "resp/200-invite",
            invite_response(
                "200 OK",
                &[
                    "Contact: <sip:carol@192.0.2.20:5060>",
                    "Record-Route: <sip:192.0.2.30;lr>, <sip:rr.example.com;lr>",
                    "Session-Expires: 1800;refresher=uac",
                    "Content-Type: application/sdp",
                ],
                ANSWER,
            ),
        ),
        // RFC 6026 §7.2 Accepted: a 2xx repeat and a fork's 2xx while the call
        // holds the transaction, then the same once the call is released.
        Seed { before: vec![invite_2xx("c3")], ..seed("resp/200-invite-repeat", invite_2xx("c3")) },
        Seed { before: vec![invite_2xx("c3")], ..seed("resp/200-invite-fork", invite_2xx("c4")) },
        Seed { release: true, ..seed("resp/200-invite-released", invite_2xx("c3")) },
        Seed {
            before: vec![invite_2xx("c3")],
            release: true,
            ..seed("resp/200-invite-released-repeat", invite_2xx("c3"))
        },
        Seed {
            before: vec![invite_2xx("c3")],
            release: true,
            ..seed("resp/200-invite-released-fork", invite_2xx("c4"))
        },
        Seed { release: true, ..seed("resp/200-delayed-offer-released", delayed_offer_2xx()) },
        seed("resp/486-invite", invite_response("486 Busy Here", &["Retry-After: 30"], "")),
        seed(
            "resp/503-invite",
            invite_response("503 Service Unavailable", &["Retry-After: 5;duration=60", "Reason: Q.850;cause=34"], ""),
        ),
        seed("resp/200-bye", bye_response("200 OK", &[])),
        seed("resp/481-bye", bye_response("481 Call/Transaction Does Not Exist", &[])),
    ]
}
