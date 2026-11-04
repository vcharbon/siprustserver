//! The projection of an inbound datagram a gate compares against: the facts
//! read off `sip-message`'s own accessors, never re-parsed here.

use pivot_schema::bundle::Arrived;
use sip_message::{HeaderName, HeaderProjection, SipMessage};

/// The facts a gate reads off an inbound datagram. Extraction lives in
/// `sip-message`; this is the projection the gate compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub method: Option<String>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub cseq_method: String,
    pub cseq: u32,
    pub call_id: String,
    pub rseq: Option<u32>,
    pub headers: HeaderProjection,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// A request's Request-URI user part — what a `ruri-pos` claim reads. `None`
    /// on a response and on a user-less URI.
    pub ruri_user: Option<String>,
    /// A request's whole Request-URI as rendered on the wire; `None` on a
    /// response.
    pub ruri: Option<String>,
    /// The From/To dialog identity, off `sip-message`'s own accessors: the two
    /// tags, the two user parts and the two whole URIs the §9 check vocabulary
    /// names.
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    pub from_user: Option<String>,
    pub to_user: Option<String>,
    pub from_uri: Option<String>,
    pub to_uri: Option<String>,
}

impl Inbound {
    /// Project a parsed message. Every field comes off `sip-message`'s own
    /// accessors — no header text is re-parsed here.
    pub fn of(msg: &SipMessage) -> Inbound {
        let headers = HeaderProjection::of(msg);
        let content_type = msg.raw(HeaderName::ContentType).next().map(str::to_string);
        let rseq = msg.raw(HeaderName::RSeq).next().and_then(|v| v.trim().parse().ok());
        match msg {
            SipMessage::Request(r) => Inbound {
                ruri_user: r.request_uri().user_identity(),
                ruri: Some(r.request_uri().to_string()),
                from_tag: r.from().tag().map(str::to_string),
                to_tag: r.to().tag().map(str::to_string),
                from_user: r.from().uri().user_identity(),
                to_user: r.to().uri().user_identity(),
                from_uri: Some(r.from().uri().to_string()),
                to_uri: Some(r.to().uri().to_string()),
                method: Some(r.method().to_string()),
                status: None,
                reason: None,
                cseq_method: r.cseq().method().to_string(),
                cseq: r.cseq().seq(),
                call_id: r.call_id().to_string(),
                rseq,
                headers,
                body: r.body().to_vec(),
                content_type,
            },
            SipMessage::Response(r) => Inbound {
                ruri_user: None,
                ruri: None,
                from_tag: r.from().tag().map(str::to_string),
                to_tag: r.to().tag().map(str::to_string),
                from_user: r.from().uri().user_identity(),
                to_user: r.to().uri().user_identity(),
                from_uri: Some(r.from().uri().to_string()),
                to_uri: Some(r.to().uri().to_string()),
                method: None,
                status: Some(r.status()),
                reason: Some(r.reason().to_string()),
                cseq_method: r.cseq().method().to_string(),
                cseq: r.cseq().seq(),
                call_id: r.call_id().to_string(),
                rseq,
                headers,
                body: r.body().to_vec(),
                content_type,
            },
        }
    }

    /// The structural identity a failure states for this datagram — the
    /// machine-readable form; its `Display` is the human one.
    pub fn arrived(&self) -> Arrived {
        match (&self.method, self.status) {
            (Some(m), _) => Arrived::Request { method: m.clone(), cseq: self.cseq },
            (None, Some(s)) => Arrived::Response {
                status: s,
                reason: self.reason.clone().unwrap_or_default(),
                cseq_method: self.cseq_method.clone(),
                cseq: self.cseq,
            },
            (None, None) => Arrived::Unreadable,
        }
    }

    /// A one-line description, for a failure that has to say what arrived.
    pub fn describe(&self) -> String {
        self.arrived().to_string()
    }

    /// The value of `name` by header IDENTITY — a compact spelling is found —
    /// first occurrence in wire order.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.first(name)
    }
}
