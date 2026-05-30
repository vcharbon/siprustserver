//! SIP message serializer (`SipMessage` -> wire bytes). Port of
//! `src/sip/Serializer.ts`.
//!
//! This is the single serialization point for all outbound SIP messages.
//! Headers are UTF-8; body is raw bytes passed through unmodified. The
//! serializer enforces Content-Length correctness at the boundary: a declared
//! length that disagrees with the body is auto-corrected (and a warning is
//! emitted), and a missing Content-Length is added when a body is present.

use crate::types::{SipHeader, SipMessage};

/// Serialize a structured SIP message to wire-format bytes.
pub fn serialize(msg: &SipMessage) -> Vec<u8> {
    let first_line = match msg {
        SipMessage::Request(r) => format!("{} {} {}", r.method, r.uri, r.version),
        SipMessage::Response(r) => format!("{} {} {}", r.version, r.status, r.reason),
    };
    let (headers, body) = match msg {
        SipMessage::Request(r) => (&r.headers, &r.body),
        SipMessage::Response(r) => (&r.headers, &r.body),
    };
    serialize_with_first_line(&first_line, headers, body)
}

/// Serialize from components (first line, headers, body), enforcing
/// Content-Length correctness.
fn serialize_with_first_line(first_line: &str, headers: &[SipHeader], body: &[u8]) -> Vec<u8> {
    let actual_length = body.len();
    // Borrow the original headers unless we must rewrite/extend them.
    let mut corrected: Option<Vec<SipHeader>> = None;

    let cl_index = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("content-length"));

    if let Some(i) = cl_index {
        let declared = headers[i].value.trim().parse::<usize>().ok();
        if declared != Some(actual_length) {
            eprintln!(
                "[Serializer] Content-Length mismatch: header={}, body={}. Auto-correcting. \
                 First line: {}",
                headers[i].value, actual_length, first_line
            );
            let mut hs = headers.to_vec();
            hs[i].value = actual_length.to_string();
            corrected = Some(hs);
        }
    } else if actual_length > 0 {
        let mut hs = headers.to_vec();
        hs.push(SipHeader {
            name: "Content-Length".to_string(),
            value: actual_length.to_string(),
        });
        corrected = Some(hs);
    }

    let effective: &[SipHeader] = corrected.as_deref().unwrap_or(headers);
    let header_lines = effective
        .iter()
        .map(|h| format!("{}: {}", h.name, h.value))
        .collect::<Vec<_>>()
        .join("\r\n");

    let mut out = format!("{first_line}\r\n{header_lines}\r\n\r\n").into_bytes();
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
        SipMessage::Request(r) => format!("{} {}", r.method, r.uri),
        SipMessage::Response(r) => format!("{} {}", r.status, r.reason),
    }
}
