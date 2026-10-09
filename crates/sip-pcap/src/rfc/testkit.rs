//! Datagram builders the detectors' tests share: a hand-written exchange goes
//! through the real flow builder and the real enrichment, so a detector is
//! always measured against a document of the shape the corpus holds.

use crate::doc::FlowsDoc;
use crate::enrich::EnrichOptions;
use crate::flow::{build_flows, FlowConfig};
use crate::{Datagram, DecodeStats};

pub(super) const A: &str = "10.0.0.1:5060";
pub(super) const B: &str = "10.0.0.2:5060";
pub(super) const P: &str = "10.0.0.9:5060";
pub(super) const C: &str = "10.0.0.3:5060";

/// The top-Via branch a message of `method` rides on the dialog `call_id`:
/// one transaction per (method, CSeq number), except a CANCEL, which reuses
/// the branch of the INVITE it cancels (RFC 3261 §9.1). The ACK of a non-2xx
/// rides its INVITE's branch too (§17.1.1.3), which [`ack_of_non_2xx`] states;
/// a pin sending two INVITEs on one CSeq number names each attempt's branch
/// with [`request_on`] and [`response_on`].
fn branch(call_id: &str, method: &str, seq: u32) -> String {
    let txn_method = if method.eq_ignore_ascii_case("CANCEL") { "INVITE" } else { method };
    format!("z9hG4bK-{call_id}-{txn_method}-{seq}")
}

pub(super) fn dg(ts_us: u64, src: &str, dst: &str, payload: String) -> Datagram {
    Datagram {
        ts_us,
        src: src.parse().unwrap(),
        dst: dst.parse().unwrap(),
        payload: payload.into_bytes(),
        probe: 0,
    }
}

/// A request whose CSeq number is `seq`, on the dialog `call_id`, with the
/// tags a real exchange would carry. `token` is the relayed `X-Api-Call` the
/// correlator groups legs by, so a b-leg states its a-leg's.
pub(super) fn request_tok(
    method: &str,
    seq: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    token: &str,
) -> String {
    request_with(method, seq, call_id, from_tag, to_tag, token, "", &branch(call_id, method, seq))
}

/// The single-leg form: the leg is its own correlation token.
pub(super) fn request(
    method: &str,
    seq: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
) -> String {
    request_tok(method, seq, call_id, from_tag, to_tag, call_id)
}

/// A request carrying `extra` header lines verbatim — how a test states the
/// `Supported`, `Require` and `RAck` headers RFC 3262 decides on.
pub(super) fn request_hdr(
    method: &str,
    seq: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    extra: &str,
) -> String {
    request_with(
        method,
        seq,
        call_id,
        from_tag,
        to_tag,
        call_id,
        extra,
        &branch(call_id, method, seq),
    )
}

/// A request on the top-Via `branch` the pin names: one attempt of several
/// that share a CSeq number.
pub(super) fn request_on(
    method: &str,
    seq: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    branch: &str,
) -> String {
    request_with(method, seq, call_id, from_tag, to_tag, call_id, "", branch)
}

/// The ACK of a non-2xx final to the INVITE numbered `seq`: it rides that
/// INVITE's branch (RFC 3261 §17.1.1.3) and carries the final's To tag.
pub(super) fn ack_of_non_2xx(seq: u32, call_id: &str, from_tag: &str, to_tag: &str) -> String {
    let branch = branch(call_id, "INVITE", seq);
    request_with("ACK", seq, call_id, from_tag, Some(to_tag), call_id, "", &branch)
}

#[allow(clippy::too_many_arguments)]
fn request_with(
    method: &str,
    seq: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    token: &str,
    extra: &str,
    branch: &str,
) -> String {
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    format!(
        "{method} sip:+1555123@h SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:001555900@h>;tag={from_tag}\r\n\
         To: <sip:+1555123@h>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {seq} {method}\r\n\
         X-Api-Call: {token}\r\n\
         {extra}Content-Length: 0\r\n\r\n"
    )
}

pub(super) fn response(
    status: u16,
    reason: &str,
    seq: u32,
    method: &str,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
) -> String {
    response_hdr(status, reason, seq, method, call_id, from_tag, to_tag, "")
}

/// A response on the top-Via `branch` the pin names — the answer to a
/// [`request_on`] attempt.
#[allow(clippy::too_many_arguments)]
pub(super) fn response_on(
    status: u16,
    reason: &str,
    seq: u32,
    method: &str,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    branch: &str,
) -> String {
    response_with(status, reason, seq, method, call_id, from_tag, to_tag, "", branch)
}

/// A response carrying `extra` header lines verbatim — `RSeq` and `Require`
/// are what make a provisional reliable (RFC 3262 §3).
#[allow(clippy::too_many_arguments)]
pub(super) fn response_hdr(
    status: u16,
    reason: &str,
    seq: u32,
    method: &str,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    extra: &str,
) -> String {
    let branch = branch(call_id, method, seq);
    response_with(status, reason, seq, method, call_id, from_tag, to_tag, extra, &branch)
}

#[allow(clippy::too_many_arguments)]
fn response_with(
    status: u16,
    reason: &str,
    seq: u32,
    method: &str,
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    extra: &str,
    branch: &str,
) -> String {
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    format!(
        "SIP/2.0 {status} {reason}\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
         From: <sip:001555900@h>;tag={from_tag}\r\n\
         To: <sip:+1555123@h>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {seq} {method}\r\n\
         {extra}Content-Length: 0\r\n\r\n"
    )
}

pub(super) fn doc_of(datagrams: Vec<Datagram>) -> FlowsDoc {
    let flows = build_flows(&datagrams, &FlowConfig::default());
    crate::emit::flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default())
        .expect("the model enriches")
}

/// `msg` carrying `body` as its session description, the head's
/// `Content-Length: 0` restated for it.
pub(super) fn with_body(msg: String, body: &str) -> String {
    msg.replace(
        "Content-Length: 0\r\n\r\n",
        &format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}", body.len()),
    )
}
