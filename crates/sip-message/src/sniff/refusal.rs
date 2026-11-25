//! Responses built from a request's own rows (RFC 3261 §8.2.6.2), with no
//! transaction state: what a UA that no longer holds a dialog answers a
//! request naming it (§12.2.2), and what it answers a CANCEL (§9.2).

use super::{header_row_spans, identity, is_response, to_tag};

/// The `status` `reason` response to the in-dialog request `raw`: its Via
/// rows, From, To (the tag it names), Call-ID and CSeq copied in wire order,
/// and no body. `None` for a response, a request whose To names no dialog,
/// or one missing any of those rows.
pub fn refusal(raw: &[u8], status: u16, reason: &str) -> Option<Vec<u8>> {
    if to_tag(raw).is_empty() {
        return None;
    }
    response_to(raw, status, reason, "", None)
}

/// The `status` `reason` response to the request `raw`, built as
/// [`refusal`] builds it, with two departures. A To that names no tag takes
/// `tag` (§8.2.6.2: a UAS response carries one); a To that names one keeps
/// it. `cseq_method`, when given, replaces the CSeq method: the 487 to the
/// INVITE a CANCEL names, built from the CANCEL (§9.2: same Via, From, To,
/// Call-ID and CSeq number). `None` for a response, a tagless To with an
/// empty `tag`, or a request missing a copied row.
pub fn response_to(
    raw: &[u8],
    status: u16,
    reason: &str,
    tag: &str,
    cseq_method: Option<&str>,
) -> Option<Vec<u8>> {
    if is_response(raw) {
        return None;
    }
    let tagless = to_tag(raw).is_empty();
    if tagless && tag.is_empty() {
        return None;
    }
    let mut out = format!("SIP/2.0 {status} {reason}\r\n").into_bytes();
    for name in ["Via", "From", "To", "Call-ID", "CSeq"] {
        let spans = header_row_spans(raw, name);
        if spans.is_empty() {
            return None;
        }
        match name {
            "To" if tagless => {
                let row = raw.get(spans.first()?.clone())?;
                out.extend_from_slice(row.trim_ascii_end());
                out.extend_from_slice(format!(";tag={tag}\r\n").as_bytes());
            }
            "CSeq" if cseq_method.is_some() => {
                let cseq = identity(raw).cseq?;
                let method = cseq_method.unwrap_or_default();
                out.extend_from_slice(format!("CSeq: {cseq} {method}\r\n").as_bytes());
            }
            _ => {
                for span in spans {
                    out.extend_from_slice(raw.get(span)?);
                }
            }
        }
    }
    out.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    Some(out)
}
