//! The RFC 3262 fields of a datagram: whether a provisional is sent
//! reliably, its RSeq, and the RAck of the PRACK acknowledging it.

use super::{header_rows_compact, header_value};

/// Whether the `Require` rows list the `100rel` option-tag, the
/// reliable-provisional marker (RFC 3262 §3), each row read by the one
/// `Require` reader: a row that does not read marks nothing, so a message
/// holding one is no reliable provisional. `Require` has no compact form.
pub fn require_has_100rel(raw: &[u8]) -> bool {
    use crate::header::{HeaderValue, Require};
    let rows = header_rows_compact(raw, "require");
    let read: Result<Vec<Require>, _> =
        rows.iter().map(|row| Require::parse(&crate::sip_str::SipStr::owned(row))).collect();
    read.is_ok_and(|lines| lines.iter().any(|line| line.contains("100rel")))
}

/// The `RSeq` value of a reliable provisional (RFC 3262 §3), or `None`.
pub fn rseq_of(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rseq")?.trim().parse().ok()
}

/// The RAck response-num (its FIRST token = the acknowledged 1xx's RSeq,
/// RFC 3262 §7.2) of a PRACK, or `None`.
pub fn rack_rseq(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rack")?.split_whitespace().next()?.parse().ok()
}

/// The RAck CSeq-num (its SECOND token = the sequence number of the INVITE the
/// acknowledged provisional answered, RFC 3262 §7.2) of a PRACK, or `None`.
///
/// `RAck` names its target by three fields, and the response-num alone does
/// not: two INVITEs on one dialog number their `RSeq` spaces independently, so
/// a reader joining a PRACK to the provisional it acknowledges needs this one
/// too.
pub fn rack_cseq(raw: &[u8]) -> Option<u32> {
    header_value(raw, "rack")?.split_whitespace().nth(1)?.parse().ok()
}
