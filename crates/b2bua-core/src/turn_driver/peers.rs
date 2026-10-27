//! The peers of a driven call: the caller alice on the a-leg, a callee on each
//! b-leg (bob on `b-1`), what each has sent and been sent, and the SIP text
//! each writes.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipHeader, SipMessage, SipParser, SipRequest};

use crate::effects::{HandlerResult, OutboundBody};

/// The offer and answer every peer makes.
pub const SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The caller's address.
pub const ALICE: &str = "10.0.0.9:5060";
/// Every callee's address.
pub const BOB: &str = "127.0.0.1:5070";
/// The caller's own tag.
pub const ALICE_TAG: &str = "alicetag";
/// The caller's Call-ID.
pub const CALL_ID: &str = "lane@10.0.0.9";

/// The To-tag the callee on `leg` answers under: `bobtag` on `b-1`.
pub fn callee_tag(leg: &str) -> String {
    if leg == "b-1" {
        "bobtag".into()
    } else {
        format!("{leg}tag")
    }
}

/// What one peer has sent and been sent.
#[derive(Clone, Debug, Default)]
pub struct Peer {
    /// The peer's last CSeq number on its dialog.
    pub cseq: u32,
    /// The CSeq of the peer's last INVITE (the caller's own).
    pub invite_cseq: u32,
    /// Every request other than an ACK the stack sent the peer, in order.
    pub heard: Vec<SipRequest>,
    /// The INVITE that dialled the peer's leg (a callee's).
    pub dialled_by: Option<SipRequest>,
}

impl Peer {
    /// The last request other than an ACK the stack sent the peer.
    pub fn last_heard(&self) -> Option<&SipRequest> {
        self.heard.last()
    }

    /// The last request of `method` the stack sent the peer.
    pub fn last_of(&self, method: &Method) -> Option<&SipRequest> {
        self.heard.iter().rev().find(|r| r.method() == method)
    }
}

/// Every peer of the call, by the leg it is reached on.
#[derive(Clone, Debug, Default)]
pub struct Peers {
    by_leg: BTreeMap<String, Peer>,
}

impl Peers {
    /// alice, who sent her INVITE, and the callee `b_invite` dialled on `b-1`.
    pub fn new(b_invite: &SipRequest) -> Self {
        let mut by_leg = BTreeMap::new();
        by_leg.insert("a".to_string(), Peer { cseq: 1, invite_cseq: 1, ..Peer::default() });
        by_leg.insert("b-1".to_string(), dialled(b_invite));
        Self { by_leg }
    }

    /// The peer on `leg`, if the stack reached it.
    pub fn on(&self, leg: &str) -> Option<&Peer> {
        self.by_leg.get(leg)
    }

    /// The peer on `leg`, to note what it sends.
    pub fn on_mut(&mut self, leg: &str) -> Option<&mut Peer> {
        self.by_leg.get_mut(leg)
    }

    /// The requests `result` sends, heard by the peer of each leg; an INVITE
    /// on a leg no peer is reached on yet dials a callee there.
    pub fn heard(&mut self, result: &HandlerResult) {
        for e in &result.effects.outbound {
            let (OutboundBody::Request(r), Some(leg)) = (&e.body, e.leg_id.as_deref()) else {
                continue;
            };
            if *r.method() == Method::Ack {
                continue;
            }
            match self.by_leg.get_mut(leg) {
                Some(peer) => peer.heard.push(r.clone()),
                None if *r.method() == Method::Invite => {
                    self.by_leg.insert(leg.to_string(), dialled(r));
                }
                None => {}
            }
        }
    }
}

/// The callee `invite` dialled.
fn dialled(invite: &SipRequest) -> Peer {
    Peer { cseq: 1, invite_cseq: 1, heard: vec![invite.clone()], dialled_by: Some(invite.clone()) }
}

/// `raw` read back; `None` when the parser refuses it, as the shell drops
/// what does not parse.
pub fn parsed(raw: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(raw).ok()
}

/// The address the peer on `leg` sends from.
pub fn address_of(leg: &str) -> Option<SocketAddr> {
    (if leg == "a" { ALICE } else { BOB }).parse().ok()
}

/// A request of `method` from `src`, its lines `lines` before the body.
pub fn request(method: &str, cseq: u32, src: &str, lines: &str, body: &str) -> Vec<u8> {
    let branch = format!("z9hG4bK-lane-{method}-{cseq}");
    request_on(method, cseq, src, &branch, lines, body)
}

/// [`request`] under the Via branch `branch`.
pub fn request_on(
    method: &str,
    cseq: u32,
    src: &str,
    branch: &str,
    lines: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "{method} sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {src};branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         {lines}\
         CSeq: {cseq} {method}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The lines a request of `method` carries beyond its dialog's and its
/// body's, from the peer reached at `contact`: a target, and what the method
/// requires.
pub fn method_lines(method: &Method, contact: &str) -> String {
    match method {
        Method::Invite | Method::Update => format!("Contact: <{contact}>\r\n"),
        Method::Refer => {
            format!("Contact: <{contact}>\r\nRefer-To: <sip:carol@127.0.0.1:5071>\r\n")
        }
        Method::Subscribe => format!("Contact: <{contact}>\r\nEvent: dialog\r\nExpires: 60\r\n"),
        Method::Notify => {
            format!("Contact: <{contact}>\r\nEvent: refer\r\nSubscription-State: active\r\n")
        }
        _ => String::new(),
    }
}

/// The body a request of `method` carries: an INVITE or an UPDATE offers.
pub fn method_body(method: &Method) -> &'static str {
    if matches!(method, Method::Invite | Method::Update) {
        SDP
    } else {
        ""
    }
}

/// The response `status` to `req` from the peer on `leg`, under the To-tag
/// `tag` when it has none: a 2xx or a 1xx to an INVITE or an UPDATE carries
/// the answer, and one that creates or refreshes a dialog the peer's Contact
/// (RFC 3261 §12.1.1, RFC 3311 §5.2, RFC 6665 §4.1.3).
pub fn response(
    req: &SipRequest,
    leg: &str,
    status: u16,
    tag: &str,
    mut extra: Vec<SipHeader>,
) -> Option<Vec<u8>> {
    let method = req.method();
    let answers = matches!(method, Method::Invite | Method::Update)
        && ((200..300).contains(&status) || (180..200).contains(&status));
    let targets =
        matches!(method, Method::Invite | Method::Update | Method::Subscribe | Method::Refer)
            && (101..300).contains(&status);
    if targets {
        extra.push(line("Contact", &format!("<{}>", contact_of(leg))));
    }
    let opts = GenerateResponseOpts {
        to_tag: req.to().tag().is_none().then(|| tag.into()),
        body: if answers { SDP.into() } else { Vec::new() },
        content_type: answers.then(crate::rules::relay::sdp),
        extra_headers: extra,
        ..Default::default()
    };
    Some(generate_response(req, status, "Lane", &opts).ok()?.image().to_vec())
}

/// The Contact of the peer on `leg`.
pub fn contact_of(leg: &str) -> String {
    if leg == "a" {
        format!("sip:alice@{ALICE}")
    } else {
        format!("sip:bob@{BOB}")
    }
}

/// The lines making a provisional reliable (RFC 3262 §3).
pub fn reliable() -> Vec<SipHeader> {
    vec![line("Require", "100rel"), line("RSeq", "1")]
}

/// The header line `name: value`.
pub fn line(name: &str, value: &str) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.to_string().into() }
}
