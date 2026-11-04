//! The seeds the proxy lane mutates, recorded off a run of the lane's own
//! proxy so they carry what the proxy decodes: a caller's dialog through a
//! worker (the INVITE, the worker's responses under the proxy's Via and its
//! branch, the caller's ACK and BYE routed by the cookie the Record-Route
//! carries, a CANCEL), a worker-outbound INVITE stating the call's other
//! owner, with the callee's responses, and worker-outbound INVITEs toward
//! names the resolver answers. Each seed names the unmutated datagrams sent before it. A
//! variant's seeds are recorded on the rig its cookies and Record-Routes
//! belong to ([`Variant::recorded_on`]). Addresses are documentation ranges
//! (RFC 5737) and private worker ones.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{CustomParser, SipMessage, SipParser, SipRequest};

use super::rig::{Peer, Rig, Variant, PROXY, PROXY_INTERNAL};

/// One seed: a stable name, the peer it leaves from, the bytes the lane
/// mutates, and the unmutated datagrams sent before and after it.
pub struct Seed {
    pub name: &'static str,
    pub from: Peer,
    pub bytes: Vec<u8>,
    pub before: Vec<(Peer, Vec<u8>)>,
    pub after: Vec<(Peer, Vec<u8>)>,
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

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0 8\r\na=sendrecv\r\n";

const CALLER_FROM: &str = "From: \"Alice\" <sip:alice@example.com>;tag=a1";
const CALLER_TO: &str = "To: <sip:bob@example.net>";
const CALLER_CALL_ID: &str = "Call-ID: lane-proxy-1@192.0.2.20";

/// The caller's INVITE every recording starts from.
pub fn caller_invite() -> Vec<u8> {
    message(
        "INVITE sip:bob@192.0.2.1:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-caller-1;rport",
            "Max-Forwards: 70",
            CALLER_FROM,
            CALLER_TO,
            CALLER_CALL_ID,
            "CSeq: 1 INVITE",
            "Contact: <sip:alice@192.0.2.20:5060>",
            "Supported: 100rel, timer",
            "Content-Type: application/sdp",
        ],
        OFFER,
    )
}

/// The caller's CANCEL of [`caller_invite`].
pub fn caller_cancel() -> Vec<u8> {
    message(
        "CANCEL sip:bob@192.0.2.1:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-caller-1;rport",
            "Max-Forwards: 70",
            CALLER_FROM,
            CALLER_TO,
            CALLER_CALL_ID,
            "CSeq: 1 CANCEL",
        ],
        "",
    )
}

fn caller_options() -> Vec<u8> {
    message(
        "OPTIONS sip:192.0.2.1:5060 SIP/2.0",
        &[
            "Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-caller-opt;rport",
            "Max-Forwards: 70",
            CALLER_FROM,
            "To: <sip:192.0.2.1:5060>",
            "Call-ID: lane-proxy-opt@192.0.2.20",
            "CSeq: 9 OPTIONS",
        ],
        "",
    )
}

/// An in-dialog request from the caller, routed by `route`.
fn caller_in_dialog(method: &str, seq: u32, route: &str, to_tag: &str) -> Vec<u8> {
    let via = format!("Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-caller-{method}-{seq}");
    let route = format!("Route: {route}");
    let to = format!("{CALLER_TO};tag={to_tag}");
    let cseq = format!("CSeq: {seq} {method}");
    message(
        &format!("{method} sip:b2b@10.0.0.2:5070 SIP/2.0"),
        &[&via, "Max-Forwards: 70", &route, CALLER_FROM, &to, CALLER_CALL_ID, &cseq],
        "",
    )
}

/// The worker owning a call beside worker a, as worker a's Via states it.
pub const BACKUP: &str = "b2b-b";

/// Callee names the resolver answers: one of even length, one of odd
/// length, which the variant with gaps resolves to nothing.
pub const NAMED: [&str; 2] = ["callee.example", "callee1.example"];

/// Worker a's INVITE toward the callee at `host`, routed through the proxy
/// at `face`, its Via stating worker b as the call's other owner.
fn worker_invite(face: (&str, u16), host: &str, call: &str) -> Vec<u8> {
    let route = format!("Route: <sip:{}:{};lr>", face.0, face.1);
    let via = format!("Via: SIP/2.0/UDP 10.0.0.2:5070;branch=z9hG4bK-worker-{call};bak={BACKUP}");
    let call_id = format!("Call-ID: lane-proxy-{call}@10.0.0.2");
    message(
        &format!("INVITE sip:carol@{host}:5060 SIP/2.0"),
        &[
            &via,
            "Max-Forwards: 70",
            &route,
            "From: <sip:b2b@10.0.0.2:5070>;tag=w1",
            "To: <sip:carol@example.org>",
            &call_id,
            "CSeq: 1 INVITE",
            "Contact: <sip:b2b@10.0.0.2:5070>",
            "Content-Type: application/sdp",
        ],
        OFFER,
    )
}

fn parse_request(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("a recorded request: {other:?}"),
    }
}

/// The response of `status` the UAS of `req` sends, its To tagged `tag`.
fn answer(req: &SipRequest, status: u16, reason: &str, tag: &str, contact: &str) -> Vec<u8> {
    let opts = GenerateResponseOpts {
        to_tag: Some(tag.into()),
        extra_headers: vec![SipHeader { name: "Contact".into(), value: contact.to_owned().into() }],
        ..GenerateResponseOpts::default()
    };
    let resp = generate_response(req, status, reason, &opts).expect("a recorded response");
    resp.image().to_vec()
}

/// The values of the `name` header lines of `raw`, in order.
fn header_values(raw: &[u8], name: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let prefix = format!("{name}:");
    text.lines()
        .filter_map(|l| l.strip_prefix(&prefix))
        .flat_map(|v| v.split(',').map(|e| e.trim().to_owned()).collect::<Vec<_>>())
        .collect()
}

/// Records the seeds on a fresh plain proxy.
pub fn all() -> Vec<Seed> {
    all_on(Variant::Plain)
}

/// Records the seeds of `variant` on a fresh proxy.
pub fn all_on(variant: Variant) -> Vec<Seed> {
    super::rig::runtime().block_on(record(variant.recorded_on()))
}

async fn record(variant: Variant) -> Vec<Seed> {
    let rig = Rig::with(variant).await;
    let face = if variant == Variant::DualFace { PROXY_INTERNAL } else { PROXY };
    let invite = caller_invite();
    rig.send(Peer::Caller, &invite).await;
    let (worker, at_worker) = [Peer::WorkerA, Peer::WorkerB]
        .into_iter()
        .find_map(|w| rig.drain(w).into_iter().next().map(|d| (w, d)))
        .expect("the proxy forwards the INVITE to a worker");
    let forwarded = parse_request(&at_worker);
    let contact = "<sip:b2b@10.0.0.2:5070>";
    let ringing = answer(&forwarded, 180, "Ringing", "b1", contact);
    let ok = answer(&forwarded, 200, "OK", "b1", contact);
    let busy = answer(&forwarded, 486, "Busy Here", "b1", contact);
    rig.send(worker, &ok).await;
    let relayed = rig.drain(Peer::Caller).pop().expect("the proxy relays the 200");
    let mut route = header_values(&relayed, "Record-Route");
    route.reverse();
    let route = route.join(", ");
    let ack = caller_in_dialog("ACK", 1, &route, "b1");
    let bye = caller_in_dialog("BYE", 2, &route, "b1");

    let outbound = worker_invite(face, "192.0.2.30", "out");
    rig.send(Peer::WorkerA, &outbound).await;
    let at_callee = rig.drain(Peer::Callee).pop().expect("the proxy forwards the outbound INVITE");
    let forwarded_out = parse_request(&at_callee);
    let callee = "<sip:carol@192.0.2.30:5060>";
    let out_ringing = answer(&forwarded_out, 180, "Ringing", "c1", callee);
    let out_ok = answer(&forwarded_out, 200, "OK", "c1", callee);

    let seed = |name, from, bytes| Seed { name, from, bytes, before: vec![], after: vec![] };
    let after_invite = vec![(Peer::Caller, invite.clone())];
    let answered = vec![(Peer::Caller, invite.clone()), (worker, ok.clone())];
    let after_outbound = vec![(Peer::WorkerA, outbound.clone())];
    vec![
        Seed {
            after: vec![(Peer::Caller, caller_cancel())],
            ..seed("req/invite", Peer::Caller, invite)
        },
        Seed { before: after_invite.clone(), ..seed("req/cancel", Peer::Caller, caller_cancel()) },
        Seed { before: answered.clone(), ..seed("req/ack", Peer::Caller, ack) },
        Seed { before: answered, ..seed("req/bye", Peer::Caller, bye) },
        Seed { before: after_invite.clone(), ..seed("resp/180-worker", worker, ringing) },
        Seed { before: after_invite.clone(), ..seed("resp/200-worker", worker, ok) },
        Seed { before: after_invite, ..seed("resp/486-worker", worker, busy) },
        seed("req/worker-invite", Peer::WorkerA, outbound),
        Seed {
            before: after_outbound.clone(),
            ..seed("resp/180-callee", Peer::Callee, out_ringing)
        },
        Seed { before: after_outbound, ..seed("resp/200-callee", Peer::Callee, out_ok) },
        seed("req/options", Peer::Caller, caller_options()),
        seed("req/worker-invite-named", Peer::WorkerA, worker_invite(face, NAMED[0], "even")),
        seed("req/worker-invite-unnamed", Peer::WorkerA, worker_invite(face, NAMED[1], "odd")),
    ]
}
