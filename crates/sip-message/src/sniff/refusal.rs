//! A refusal built from a request's own rows (RFC 3261 §8.2.6.2): what a UA
//! that no longer holds a dialog answers a request naming it (§12.2.2).

use super::{header_row_spans, is_response, to_tag};

/// The `status` `reason` response to the in-dialog request `raw`: its Via
/// rows, From, To (the tag it names), Call-ID and CSeq copied in wire order,
/// and no body. `None` for a response, a request whose To names no dialog,
/// or one missing any of those rows.
pub fn refusal(raw: &[u8], status: u16, reason: &str) -> Option<Vec<u8>> {
    if is_response(raw) || to_tag(raw).is_empty() {
        return None;
    }
    let mut out = format!("SIP/2.0 {status} {reason}\r\n").into_bytes();
    for name in ["Via", "From", "To", "Call-ID", "CSeq"] {
        let spans = header_row_spans(raw, name);
        if spans.is_empty() {
            return None;
        }
        spans.into_iter().for_each(|s| out.extend_from_slice(&raw[s]));
    }
    out.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    Some(out)
}
