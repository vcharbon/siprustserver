//! Two copies of one message compared outside what each sender states on its
//! own: the rows of a named header field, the stamps a relay leaves on the Via
//! of the hop it received the request from; and the line a difference sits in.

use std::ops::Range;

use super::{as_str, relay_stamp_spans};
use crate::parser::custom::compact_forms::expanded_name;

/// What [`equal_except`] leaves out of each copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Except<'a> {
    /// Every row of the header field of this name, long or compact form
    /// (RFC 3261 §7.3.3, case-insensitive), its continuation lines and line
    /// ending included.
    Header(&'a str),
    /// The `received` and `rport` stamps on the second Via
    /// ([`relay_stamp_spans`]).
    RelayStamps,
}

/// Whether `a` and `b` are one datagram byte for byte once what `except`
/// names is taken out of each.
pub fn equal_except(a: &[u8], b: &[u8], except: &[Except<'_>]) -> bool {
    let (a_spans, b_spans) = (spans(a, except), spans(b, except));
    kept(a, &a_spans).eq(kept(b, &b_spans))
}

/// The spans of every row of header `name` in the header block of `raw`, in
/// wire order: from the row's first byte through the line ending of its last
/// continuation line (RFC 3261 §7.3.1). The start line is no row.
pub fn header_row_spans(raw: &[u8], name: &str) -> Vec<Range<usize>> {
    let name = expanded_name(name);
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut in_named_row = false;
    let mut pos = raw.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| p + 1);
    while let Some(rest) = raw.get(pos..).filter(|rest| !rest.is_empty()) {
        let next = rest.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| pos + p + 1);
        let line = raw.get(pos..next).unwrap_or_default();
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content.is_empty() {
            break; // end of headers
        }
        if matches!(content.first(), Some(b' ' | b'\t')) {
            if in_named_row {
                if let Some(last) = spans.last_mut() {
                    last.end = next;
                }
            }
        } else {
            let row_name = content.split(|&b| b == b':').next().unwrap_or_default();
            let row_name = String::from_utf8_lossy(row_name);
            in_named_row = expanded_name(row_name.trim()).eq_ignore_ascii_case(name);
            if in_named_row {
                spans.push(pos..next);
            }
        }
        pos = next;
    }
    spans
}

/// The line of `raw` that spans byte `offset` — the header row (or, past the
/// blank line, the body line) a byte-level divergence between two copies of one
/// message sits in — without its line ending. `None` where `offset` is past the
/// end of `raw`, which is where a copy that is a strict prefix of another stops.
pub fn line_at(raw: &[u8], offset: usize) -> Option<String> {
    if offset >= raw.len() {
        return None;
    }
    let start = raw.get(..offset)?.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let end = raw.get(offset..)?.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| offset + p);
    Some(as_str(raw.get(start..end)?).trim_end_matches('\r').to_string())
}

/// The spans of `raw` that `except` names, sorted and merged where they
/// overlap.
fn spans(raw: &[u8], except: &[Except<'_>]) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = except
        .iter()
        .flat_map(|e| match e {
            Except::Header(name) => header_row_spans(raw, name),
            Except::RelayStamps => relay_stamp_spans(raw),
        })
        .collect();
    spans.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The bytes of `raw` outside `spans` (sorted, disjoint), in order.
fn kept<'r>(raw: &'r [u8], spans: &[Range<usize>]) -> impl Iterator<Item = u8> + 'r {
    let mut pieces: Vec<&'r [u8]> = Vec::with_capacity(spans.len() + 1);
    let mut at = 0;
    for span in spans {
        pieces.push(raw.get(at..span.start).unwrap_or_default());
        at = span.end;
    }
    pieces.push(raw.get(at..).unwrap_or_default());
    pieces.into_iter().flatten().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel(reason: &str, uri: &str, stamp: &str) -> Vec<u8> {
        format!(
            "CANCEL sip:{uri} SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bKp\r\n\
             Via: SIP/2.0/UDP 192.0.2.1:5092;branch=z9hG4bKw{stamp}\r\nCall-ID: c\r\n\
             CSeq: 1 CANCEL\r\n{reason}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn the_rows_of_the_named_header_are_left_out() {
        let a = cancel("", "b@x", "");
        let b = cancel("Reason: Q.850;cause=16\r\n", "b@x", "");
        assert!(equal_except(&a, &b, &[Except::Header("Reason")]));
        assert!(!equal_except(&a, &b, &[]));
        assert!(!equal_except(&a, &b, &[Except::RelayStamps]));
    }

    #[test]
    fn a_difference_elsewhere_still_counts() {
        let a = cancel("", "b@x", "");
        let b = cancel("Reason: Q.850;cause=16\r\n", "c@x", "");
        assert!(!equal_except(&a, &b, &[Except::Header("Reason"), Except::RelayStamps]));
    }

    #[test]
    fn the_relay_stamps_are_left_out() {
        let a = cancel("", "b@x", ";rport=5092;received=192.0.2.1");
        let b = cancel("Reason: Q.850;cause=16\r\n", "b@x", ";rport=5093;received=192.0.2.2");
        assert!(equal_except(&a, &b, &[Except::Header("Reason"), Except::RelayStamps]));
        assert!(!equal_except(&a, &b, &[Except::Header("Reason")]));
    }

    #[test]
    fn a_compact_form_and_a_continuation_line_belong_to_the_row() {
        let raw = b"BYE sip:x SIP/2.0\r\nv: SIP/2.0/UDP a;branch=z9hG4bK1\r\nSubject: one\r\n two\r\nCall-ID: c\r\n\r\nv: body\r\n";
        let rows: Vec<&[u8]> = header_row_spans(raw, "Via").into_iter().map(|r| &raw[r]).collect();
        assert_eq!(rows, [b"v: SIP/2.0/UDP a;branch=z9hG4bK1\r\n".as_slice()]);
        let rows: Vec<&[u8]> = header_row_spans(raw, "s").into_iter().map(|r| &raw[r]).collect();
        assert_eq!(rows, [b"Subject: one\r\n two\r\n".as_slice()]);
    }

    #[test]
    fn the_start_line_is_no_row() {
        assert!(header_row_spans(b"Reason: x SIP/2.0\r\nCall-ID: c\r\n\r\n", "Reason").is_empty());
    }
}
