//! The measured SIP-stack operations and the fixtures they run on.
//!
//! One definition, two consumers: `tests/alloc_budget.rs` counts allocations
//! per operation and `benches/sip_parser.rs` times the same operations on the
//! same bytes, so the allocation story and the wall-clock story describe one
//! set of cases.
//!
//! The cases are the paths ADR-0025 puts a guardrail on:
//!
//! - `decode/*` — parse only (raw bytes → `SipMessage`).
//! - `hop/*` — the thawed-draft edit alone, on a message parsed outside the
//!   measured region: the full proxy rewrite set, and the received/rport stamp
//!   on its own.
//! - `proxy_hop/*` — decode plus one hop, the per-inbound-datagram cost.
//! - `build/*` — origination: the blank draft driven directly, and the
//!   generator recipes over typed options.

use sip_message::draft::RequestDraft;
use sip_message::generators::{
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts, StackDialog,
};
use sip_message::header::{
    self, CSeq, CallId, Contact, MaxForwards, MediaType, ParamValue, RecordRouteEntry, RouteEntry,
    To, Uri, Via,
};
use sip_message::{Bytes, CustomParser, Method, SipMessage, SipParser, SipRequest, SipStr};

pub const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

pub const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:bob@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

/// The advertised address of the proxy the hop cases forward through — the
/// host the Route entries they pop and the Record-Routes they push all name.
const PROXY_HOST: &str = "proxy.example.com";
const PROXY_PORT: u16 = 5060;

/// The SDP offer the INVITE fixtures carry.
pub fn sdp_offer() -> &'static str {
    "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n"
}

/// An INVITE carrying an SDP offer — what a proxy actually forwards. Built at
/// runtime so Content-Length matches the body exactly.
pub fn invite_with_sdp() -> Vec<u8> {
    let sdp = sdp_offer();
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
        sdp.len(),
        sdp
    )
    .into_bytes()
}

/// The in-dialog INVITE the full rewrite set runs on: a confirmed dialog whose
/// route set starts with the two entries this proxy recorded, so the hop pops
/// its own halves before it forwards.
pub fn in_dialog_invite_with_sdp() -> Vec<u8> {
    let sdp = sdp_offer();
    format!(
        "INVITE sip:bob@192.0.2.99:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123\r\n\
Max-Forwards: 70\r\n\
Route: <sip:{PROXY_HOST}:{PROXY_PORT};v=3;w_pri=b2b-1;lr>\r\n\
Route: <sip:{PROXY_HOST}:{PROXY_PORT};outbound;lr>\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314160 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
        sdp.len(),
        sdp
    )
    .into_bytes()
}

/// The request a case parses out of `raw`, or a panic — every fixture here is
/// a request the parser accepts.
pub fn request(parser: &CustomParser, raw: &[u8]) -> SipRequest {
    match parser.parse(raw).expect("fixture parses") {
        SipMessage::Request(req) => req,
        SipMessage::Response(_) => panic!("fixture is a request"),
    }
}

/// One minimal forwarding hop: thaw → rewrite the Request-URI → record our
/// route → render.
pub fn hop_minimal(req: &SipRequest) -> Bytes {
    req.thaw()
        .with_uri(Uri::parse_or_verbatim(&SipStr::from_static("sip:bob@192.0.2.99:5060")))
        .push_front(RecordRouteEntry::from_uri(Uri::sip(PROXY_HOST).with_flag("lr")))
        .freeze_bytes()
        .expect("a thawed draft is complete")
}

/// The full rewrite set one forwarding hop performs (RFC 3261 §16.4/§16.6):
/// pop the Route entries this proxy recorded, stamp received/rport on the top
/// Via, state the decremented Max-Forwards, record the direction-carrying
/// Record-Route pair, and push this hop's own Via.
pub fn hop_rewrite_set(req: &SipRequest) -> Bytes {
    let routes: Vec<RouteEntry> = req.list::<RouteEntry>().unwrap_or_default();
    let ours = routes.iter().take_while(|r| r.uri().host() == PROXY_HOST).count();
    let mut draft = req.thaw();
    for _ in 0..ours {
        draft = draft.pop_top::<RouteEntry>().expect("our own Route entries read");
    }
    draft = draft
        .update_top::<Via>(|via| via.stamped_from("192.0.2.10", 33000))
        .expect("the top Via reads");
    draft = draft.set(MaxForwards::new(69));
    draft = draft.push_front(RecordRouteEntry::from_uri(
        Uri::sip(PROXY_HOST).with_port(PROXY_PORT).with_flag("outbound").with_flag("lr"),
    ));
    draft = draft.push_front(RecordRouteEntry::from_uri(
        Uri::sip(PROXY_HOST)
            .with_port(PROXY_PORT)
            .with_param("w_pri", ParamValue::text("b2b-1"))
            .with_flag("lr"),
    ));
    draft
        .push_front(
            Via::udp(PROXY_HOST, PROXY_PORT).with_branch("z9hG4bK-hop2").requesting_rport(),
        )
        .freeze_bytes()
        .expect("a thawed draft is complete")
}

/// The received/rport stamp on its own (RFC 3261 §18.2.1, RFC 3581 §4) — the
/// smallest edit a hop can make, and the floor a thawed-draft hop costs.
pub fn hop_stamp_received_rport(req: &SipRequest) -> Bytes {
    req.thaw()
        .update_top::<Via>(|via| via.stamped_from("192.0.2.10", 33000))
        .expect("the top Via reads")
        .freeze_bytes()
        .expect("a thawed draft is complete")
}

/// The typed values one blank-draft origination pushes. Held so the measured
/// region charges the draft, not the caller minting its own dialog identity —
/// the same convention the generator cases use for their options. Cloning a
/// value copies no text: `SipStr` clones are refcount bumps and a one- or
/// two-parameter list lives inline.
pub struct BlankInvite {
    pub uri: Uri,
    pub via: Via,
    pub from: header::From,
    pub to: To,
    pub call_id: CallId,
    pub cseq: CSeq,
    pub contact: Contact,
    pub content_type: MediaType,
    pub body: Bytes,
}

impl BlankInvite {
    /// The same INVITE the `decode/invite_sdp` fixture carries, as values.
    pub fn new(sdp: &[u8]) -> Self {
        Self {
            uri: Uri::sip_user("bob", "example.com"),
            via: Via::udp("10.0.0.1", 5060)
                .with_branch("z9hG4bK-abc123")
                .with_param("cr", ParamValue::text("cref1")),
            from: header::From::from_uri(Uri::sip_user("alice", "example.com"))
                .with_display("Alice")
                .with_tag("1928"),
            to: To::from_uri(Uri::sip_user("bob", "example.com")).with_display("Bob"),
            call_id: CallId::new("a84b4c76e66710@pc33.example.com"),
            cseq: CSeq::new(314159, Method::Invite),
            contact: Contact::from_uri(
                Uri::sip_user("b2bua", "10.0.0.1")
                    .with_port(5060)
                    .with_param("callRef", ParamValue::text("cref1")),
            ),
            content_type: MediaType::new("application/sdp"),
            body: Bytes::copy_from_slice(sdp),
        }
    }

    /// Origination straight onto the draft: no options struct, no string
    /// assembly, one render.
    pub fn build(&self) -> SipRequest {
        RequestDraft::new(Method::Invite, self.uri.clone())
            .push(self.via.clone())
            .push(MaxForwards::new(70))
            .push(self.from.clone())
            .push(self.to.clone())
            .push(self.call_id.clone())
            .push(self.cseq.clone())
            .push(self.contact.clone())
            .body(self.body.clone(), self.content_type.clone())
            .freeze()
            .expect("a blank draft that states every mandatory header freezes")
    }
}

/// The Via, Contact and dialog the generator build cases originate from — the
/// shape a B2BUA b-leg carries (custom correlation params on both).
pub fn build_via() -> Via {
    Via::udp("10.0.0.1", 5060)
        .with_branch("z9hG4bK-abc123")
        .with_param("cr", ParamValue::Token(SipStr::from_static("cref1")))
}

pub fn build_contact() -> header::Contact {
    header::Contact::from_uri(
        Uri::sip_user("b2bua", "10.0.0.1")
            .with_port(5060)
            .with_param("callRef", ParamValue::Token(SipStr::from_static("cref1"))),
    )
}

pub fn build_dialog() -> StackDialog {
    StackDialog {
        call_id: "a84b4c76e66710@pc33.example.com".to_string(),
        local_tag: "1928".to_string(),
        remote_tag: "as83kf".to_string(),
        local_uri: "sip:alice@example.com".to_string(),
        remote_uri: "sip:bob@example.com".to_string(),
        remote_target: "sip:bob@192.0.2.99:5060".to_string(),
        local_cseq: 314159,
        route_set: vec!["<sip:proxy.example.com;lr>".to_string()],
    }
}

/// The options one generator origination is built from: an INVITE carrying the
/// same SDP offer the decode cases parse. Built once, so the measurement is the
/// construction cost and not the caller's own bookkeeping.
pub fn invite_opts(sdp: &[u8]) -> GenerateOutOfDialogRequestOpts {
    GenerateOutOfDialogRequestOpts {
        request_uri: Some(Uri::sip_user("bob", "example.com")),
        call_id: "a84b4c76e66710@pc33.example.com".to_string(),
        from: Some(
            header::From::from_uri(Uri::sip_user("alice", "example.com"))
                .with_tag(SipStr::from_static("1928")),
        ),
        to: Some(header::To::from_uri(Uri::sip_user("bob", "example.com"))),
        cseq: 314159,
        via: Some(build_via()),
        contact: Some(build_contact()),
        body: sdp.to_vec(),
        ..Default::default()
    }
}

pub fn bye_opts() -> GenerateInDialogRequestOpts {
    GenerateInDialogRequestOpts { via: Some(build_via()), ..Default::default() }
}

pub fn response_opts() -> GenerateResponseOpts {
    GenerateResponseOpts {
        to_tag: Some("as83kf".to_string()),
        contact: Some(build_contact()),
        incoming_source: Some(("192.0.2.10".to_string(), 33000)),
        ..Default::default()
    }
}

/// The SDP body of a rendered INVITE fixture.
pub fn body_of(raw: &[u8]) -> Vec<u8> {
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("blank line") + 4;
    raw[at..].to_vec()
}
