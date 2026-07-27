//! SIP message serializer (`SipMessage` -> wire bytes). Port of
//! `src/sip/Serializer.ts`.
//!
//! This is the single serialization point for all outbound SIP messages.
//! Headers are UTF-8; body is raw bytes passed through unmodified. The
//! serializer enforces Content-Length correctness at the boundary: a declared
//! length that disagrees with the body is auto-corrected (and a warning is
//! emitted), and a missing Content-Length is added when a body is present.

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
pub(crate) fn render(
    headers: &[SipHeader],
    body: &[u8],
    start_line: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    let mut out = Vec::with_capacity(wire_size(headers, body.len()));
    start_line(&mut out);
    finish(out, headers, body)
}

pub(crate) fn write_request_line(out: &mut Vec<u8>, req: &SipRequest) {
    use std::io::Write;
    let _ = write!(out, "{} {} {}", req.method(), req.request_uri().text(), req.version());
}

pub(crate) fn write_status_line(out: &mut Vec<u8>, resp: &SipResponse) {
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
    let first_line_end = out.len();
    let cl_index = headers.iter().position(|h| h.name.eq_ignore_ascii_case("content-length"));
    let declared_ok = match cl_index {
        Some(i) => headers[i].value.trim().parse::<usize>().ok() == Some(actual_length),
        None => actual_length == 0,
    };
    if let Some(i) = cl_index {
        if !declared_ok {
            eprintln!(
                "[Serializer] Content-Length mismatch: header={}, body={actual_length}. \
                 Auto-correcting. First line: {}",
                headers[i].value,
                String::from_utf8_lossy(&out[..first_line_end])
            );
        }
    }

    out.extend_from_slice(b"\r\n");
    for (i, h) in headers.iter().enumerate() {
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
    // TS: Math.min(indexOf 0x0d, indexOf 0x0a, 200), where a missing byte is -1.
    // Mirror that exactly: -1 wins the min and falls back to min(len, 200).
    let end = [cr.map(|n| n as isize).unwrap_or(-1), lf.map(|n| n as isize).unwrap_or(-1), 200]
        .into_iter()
        .min()
        .unwrap();
    let cut = if end > 0 { end as usize } else { raw.len().min(200) };
    String::from_utf8_lossy(&raw[..cut.min(raw.len())]).into_owned()
}

/// One-line summary from a structured message (no buffer needed).
pub fn message_summary(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!("{} {}", r.method(), r.request_uri().text()),
        SipMessage::Response(r) => format!("{} {}", r.status(), r.reason()),
    }
}
