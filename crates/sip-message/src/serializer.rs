//! SIP message serializer (`SipMessage` -> wire bytes).
//!
//! Headers are UTF-8; body is raw bytes passed through unmodified. The
//! serializer enforces Content-Length correctness at the boundary: exactly one
//! Content-Length leaves, at the place of the first row stating one (in either
//! name form), restated to the body's length when it disagrees; a later row is
//! dropped; one is added when none is stated and a body is present.

use crate::header::HeaderName;
use crate::types::{SipHeader, SipMessage, SipRequest, SipResponse};

/// Serialize a structured SIP message to wire-format bytes.
pub fn serialize(msg: &SipMessage) -> Vec<u8> {
    match msg {
        SipMessage::Request(r) => render(r.headers(), r.body(), |out| write_request_line(out, r)),
        SipMessage::Response(r) => render(r.headers(), r.body(), |out| write_status_line(out, r)),
    }
}

/// Render a message whose header block is stated by the caller — the template
/// lane's emission seam, where the capture's header-name spelling and
/// remote-target rewrites are applied to the block a generator produced.
pub fn render(
    headers: &[SipHeader],
    body: &[u8],
    start_line: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    let mut out = Vec::with_capacity(wire_size(headers, body.len()));
    start_line(&mut out);
    finish(out, headers, body)
}

/// The request line of `req`, as the wire writes it.
pub fn write_request_line(out: &mut Vec<u8>, req: &SipRequest) {
    use std::io::Write;
    let _ = write!(out, "{} {} {}", req.method(), req.request_uri().text(), req.version());
}

/// The status line of `resp`, as the wire writes it.
pub fn write_status_line(out: &mut Vec<u8>, resp: &SipResponse) {
    use std::io::Write;
    let _ = write!(out, "{} {} {}", resp.version(), resp.status(), resp.reason());
}

/// Exact-enough capacity for the whole datagram, so a serialization is ONE
/// allocation: no per-header `format!`, no intermediate join.
fn wire_size(headers: &[SipHeader], body_len: usize) -> usize {
    const FIRST_LINE_HINT: usize = 96;
    let header_bytes: usize =
        headers.iter().map(|h| h.name.len() + h.value.len() + 4).sum::<usize>();
    FIRST_LINE_HINT + header_bytes + 32 + body_len
}

/// Append the header block + body to a buffer already holding the first line,
/// enforcing Content-Length correctness at this single boundary.
fn finish(mut out: Vec<u8>, headers: &[SipHeader], body: &[u8]) -> Vec<u8> {
    use std::io::Write;

    let actual_length = body.len();
    let is_content_length =
        |h: &SipHeader| HeaderName::known(&h.name) == Some(HeaderName::ContentLength);
    let cl = headers.iter().enumerate().find(|(_, h)| is_content_length(h));
    let cl_index = cl.map(|(i, _)| i);
    let declared_ok = match cl {
        Some((_, h)) => h.value.trim().parse::<usize>().ok() == Some(actual_length),
        None => actual_length == 0,
    };

    out.extend_from_slice(b"\r\n");
    for (i, h) in headers.iter().enumerate() {
        if cl_index != Some(i) && is_content_length(h) {
            continue;
        }
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        if cl_index == Some(i) && !declared_ok {
            let _ = write!(out, "{actual_length}");
        } else {
            out.extend_from_slice(h.value.as_bytes());
        }
        out.extend_from_slice(b"\r\n");
    }
    if cl_index.is_none() && actual_length > 0 {
        let _ = write!(out, "Content-Length: {actual_length}\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// One-line summary of a raw SIP message for debug logging (first line only).
pub fn sip_summary(raw: &[u8]) -> String {
    let cr = raw.iter().position(|&b| b == 0x0d);
    let lf = raw.iter().position(|&b| b == 0x0a);
    // The first line, cut at 200 bytes, when the datagram holds a CR and an
    // LF and neither opens it; otherwise its first 200 bytes.
    let cut = match (cr, lf) {
        (Some(cr), Some(lf)) if cr.min(lf) > 0 => cr.min(lf).min(200),
        _ => raw.len().min(200),
    };
    let head = raw.split_at_checked(cut.min(raw.len())).map_or(raw, |(head, _)| head);
    String::from_utf8_lossy(head).into_owned()
}

/// One-line summary from a structured message (no buffer needed).
pub fn message_summary(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!("{} {}", r.method(), r.request_uri().text()),
        SipMessage::Response(r) => format!("{} {}", r.status(), r.reason()),
    }
}
