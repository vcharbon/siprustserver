//! The digit runs of a datagram the number pass maps beyond the numbers the
//! harvest reads: every run of a phone number's width ([`PHONE_WIDTH`]) or
//! more that is no protocol counter ([`fields::is_counter`], an SDP `o=`
//! sess-id or sess-version) and no part of a dialog or transaction identifier
//! (the identifier pass's) — in a charging id, a vendor header, an SDP
//! attribute, a control document — so an identity
//! written where no harvest looks is mapped like any number, the same way at
//! every occurrence. Left out are zeros only (they name no one), an address
//! literal (the address pass's), a
//! PASSporT and a User-to-User payload (their own passes'), protocol text
//! (never anonymised, [`crate::rewrite::protocol_spans`]), and a body part
//! the anonymiser redacts whole. A percent escape's hex digits (`%40`) belong
//! to no run.

use std::ops::Range;

use sip_message::SipMessage;

use super::bodies::{parts_of, understood};
use super::BodyTypes;
use crate::rewrite::fields::{digit_runs, header_name, is_counter};
use crate::rewrite::identifiers::is_identifier;
use crate::rewrite::{addresses, origin_field};

/// The narrowest free-standing run read as a number: a phone number's
/// width. A shorter value is a counter or a code more often than a number,
/// and its small fake space would merge distinct values.
pub const PHONE_WIDTH: usize = 6;

/// Every such run of `raw` — a datagram the parser refuses included, read
/// whole — in wire order, outside the pieces that have a pass of their own:
/// a User-to-User or PASSporT payload (`payloads`) where it stands whole in
/// its own header, a display name (`displays`) where it stands as one —
/// quoted, or followed by optional whitespace and `<` — and nowhere else.
pub(super) fn runs(
    msg: Option<&SipMessage>,
    raw: &[u8],
    types: &BodyTypes,
    payloads: &[&[u8]],
    displays: &[&[u8]],
) -> Vec<String> {
    let mut skipped: Vec<Range<usize>> =
        addresses::literals(raw).into_iter().map(|(start, end)| start..end).collect();
    // Protocol text is never anonymised, so never read.
    skipped.extend(crate::rewrite::protocol_spans::protocol_spans(raw, types));
    let pieces = payloads.iter().map(|p| (p, false)).chain(displays.iter().map(|d| (d, true)));
    for (piece, display) in pieces.filter(|(p, _)| !p.is_empty()) {
        let mut at = 0;
        while let Some(p) = raw[at..].windows(piece.len()).position(|w| w == *piece) {
            let (start, end) = (at + p, at + p + piece.len());
            // Where the piece stands whole, never where its bytes spell part
            // of something else.
            let edge = |b: Option<&u8>| b.is_none_or(|b| !b.is_ascii_alphanumeric());
            let whole = edge(start.checked_sub(1).and_then(|i| raw.get(i))) && edge(raw.get(end));
            let own_site =
                if display { display_form(raw, start, end) } else { payload_site(raw, start) };
            if whole && own_site {
                skipped.push(start..end);
            }
            at = end;
        }
    }
    let parts = msg.map(|msg| parts_of(msg, raw)).unwrap_or_default();
    for part in parts.into_iter().filter(|p| !understood(p, raw, types)) {
        skipped.push(part.start..part.end);
        skipped.extend(part.headers.into_iter().map(|(_, span)| span));
    }
    let mut out = Vec::new();
    for Range { start: i, end } in digit_runs(raw) {
        let own_pass = match header_name(raw, i).as_deref() {
            Some("user-to-user") => true,
            // The PASSporT is its pass's; the parameters behind it (`info`) are not.
            Some("identity") => before_first_param(raw, i),
            // A device id is the names pass's, faked whole.
            _ => in_device_id(raw, i),
        };
        let kept = end - i < PHONE_WIDTH
            || is_identifier(raw, i, end)
            || raw[i..end].iter().all(|b| *b == b'0')
            || own_pass
            || skipped.iter().any(|s| s.start < end && i < s.end)
            || is_counter(raw, i, end)
            || origin_field(raw, i, end).is_some();
        if !kept {
            out.push(String::from_utf8_lossy(&raw[i..end]).into_owned());
        }
    }
    out
}

/// Whether `raw[start..end]` stands as a display name: between quotes, or
/// followed by optional whitespace and `<` (the display masking's two forms).
fn display_form(raw: &[u8], start: usize, end: usize) -> bool {
    let quoted = start >= 1 && raw[start - 1] == b'"' && raw.get(end) == Some(&b'"');
    let bracket = raw[end..].iter().find(|b| !matches!(b, b' ' | b'\t'));
    quoted || bracket == Some(&b'<')
}

/// Whether `raw[at]` stands where a payload has its own pass: a
/// User-to-User or `Identity` header, or an escaped `User-to-User` URI header.
fn payload_site(raw: &[u8], at: usize) -> bool {
    matches!(header_name(raw, at).as_deref(), Some("user-to-user" | "identity"))
        || crate::rewrite::identifiers::param_name(raw, at).as_deref() == Some("user-to-user")
}

/// Whether `raw[at]` stands in the value of a `+sip.instance` parameter.
fn in_device_id(raw: &[u8], at: usize) -> bool {
    const PARAM: &[u8] = b"+sip.instance=";
    let line_start = raw[..at].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let line = &raw[line_start..at];
    let Some(p) = line.windows(PARAM.len()).rposition(|w| w.eq_ignore_ascii_case(PARAM)) else {
        return false;
    };
    let value = &line[p + PARAM.len()..];
    let quoted = value.first() == Some(&b'"');
    let closed =
        value
            .iter()
            .skip(1)
            .any(|b| if quoted { *b == b'"' } else { matches!(b, b';' | b',' | b'>') });
    !closed
}

/// Whether `raw[at]` stands in its header value ahead of the first `;`.
fn before_first_param(raw: &[u8], at: usize) -> bool {
    let line_start = raw[..at].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    !raw[line_start..at].contains(&b';')
}
