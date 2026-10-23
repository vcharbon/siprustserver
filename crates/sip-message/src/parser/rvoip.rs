//! The parity oracle (ADR-0001): `rvoip-sip-core`, a second SIP parser linked
//! only under the `rvoip-oracle` feature. It reads a datagram into the same
//! [`Reading`] the production parser's message projects to, so a differential
//! test compares the two verdict by verdict and field by field.

use std::net::IpAddr;

use rvoip_sip_core::parser::message::{parse_message_with_mode, ParseMode};
use rvoip_sip_core::types::sip_message::Message;
use rvoip_sip_core::types::TypedHeader;

use crate::types::SipMessage;

/// The fields a differential test compares: the start line, the headers every
/// request carries (RFC 3261 §8.1.1), and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub start: Start,
    pub call_id: Option<String>,
    /// The CSeq sequence number and method name.
    pub cseq: Option<(u32, String)>,
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    /// Every Via value, topmost first.
    pub vias: Vec<Hop>,
    /// The Contact entries, none for the wildcard.
    pub contacts: usize,
    /// The Route entries. `None` when the production parser cannot read a
    /// value; the oracle counts the entries it read, always `Some`.
    pub routes: Option<usize>,
    /// The Record-Route entries, `None` as for [`routes`](Self::routes).
    pub record_routes: Option<usize>,
    pub body: Vec<u8>,
}

/// The start line: a request's method and Request-URI, or a response's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    Request { method: String, uri: String },
    Response { status: u16 },
}

/// One Via value: its transport, sent-by and branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// Upper-cased: the transport token is case-insensitive (RFC 3261 §7.3.1).
    pub transport: String,
    /// An IP address in its canonical text, an IPv6 one bracketed; a domain
    /// name as written.
    pub host: String,
    pub port: Option<u16>,
    pub branch: Option<String>,
}

/// rvoip's strict-mode reading of `raw`, or its refusal.
pub fn read(raw: &[u8]) -> Result<Reading, String> {
    let message =
        parse_message_with_mode(raw, ParseMode::Strict).map_err(|e| format!("rvoip: {e}"))?;
    let start = match &message {
        Message::Request(req) => {
            Start::Request { method: req.method.to_string(), uri: req.uri.to_string() }
        }
        Message::Response(resp) => Start::Response { status: resp.status.as_u16() },
    };
    let headers = message.all_headers();
    let cseq = headers.iter().find_map(|h| match h {
        TypedHeader::CSeq(c) => Some((c.seq, c.method.to_string())),
        _ => None,
    });
    let vias = headers
        .iter()
        .filter_map(|h| match h {
            TypedHeader::Via(via) => Some(via.headers()),
            _ => None,
        })
        .flatten()
        .map(|v| Hop {
            transport: v.transport().to_ascii_uppercase(),
            host: canonical_host(&v.host().to_string()),
            port: v.port(),
            branch: v.branch().map(str::to_string),
        })
        .collect();
    let count = |entries: fn(&TypedHeader) -> Option<usize>| -> usize {
        headers.iter().filter_map(entries).sum()
    };
    Ok(Reading {
        start,
        call_id: message.call_id().map(|c| c.as_str().to_string()),
        cseq,
        from_tag: message.from().and_then(|f| f.tag()).map(str::to_string),
        to_tag: message.to().and_then(|t| t.tag()).map(str::to_string),
        vias,
        contacts: count(|h| match h {
            TypedHeader::Contact(c) => Some(c.addresses().count()),
            _ => None,
        }),
        routes: Some(count(|h| match h {
            TypedHeader::Route(r) => Some(r.0.len()),
            _ => None,
        })),
        record_routes: Some(count(|h| match h {
            TypedHeader::RecordRoute(r) => Some(r.0.len()),
            _ => None,
        })),
        body: message.body().to_vec(),
    })
}

impl Reading {
    /// The same fields of a message the production parser accepted.
    pub fn of(msg: &SipMessage) -> Self {
        let start = match msg {
            SipMessage::Request(req) => Start::Request {
                method: req.method().to_string(),
                uri: req.request_uri().to_string(),
            },
            SipMessage::Response(resp) => Start::Response { status: resp.status() },
        };
        Self {
            start,
            call_id: Some(msg.call_id().as_str().to_string()),
            cseq: Some((msg.cseq().seq(), msg.cseq().method().to_string())),
            from_tag: msg.from().tag().map(str::to_string),
            to_tag: msg.to().tag().map(str::to_string),
            vias: msg
                .via()
                .iter()
                .map(|v| Hop {
                    transport: v.transport().to_ascii_uppercase(),
                    host: canonical_host(v.host()),
                    port: v.port(),
                    branch: v.branch().map(str::to_string),
                })
                .collect(),
            contacts: msg.contacts().as_slice().len(),
            routes: msg.route_set().ok().map(|r| r.into_iter().count()),
            record_routes: msg.record_route_set().ok().map(|r| r.into_iter().count()),
            body: msg.body().to_vec(),
        }
    }
}

/// `host` with an IP address in canonical text, so the two parsers' renderings
/// of one address compare equal.
fn canonical_host(host: &str) -> String {
    match crate::ip_literal::ip_host(host) {
        Some(IpAddr::V6(v6)) => format!("[{v6}]"),
        Some(IpAddr::V4(v4)) => v4.to_string(),
        None => host.to_string(),
    }
}
