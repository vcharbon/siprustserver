//! The render pass: a draft becomes one pre-sized buffer, and the frozen
//! message's fields become spans of it.
//!
//! Raw entries are memcpy'd from whatever image they point into; typed entries
//! render straight into the same buffer. Nothing is re-parsed and no value gets
//! its own `String`.

use crate::header::Wire;

use super::entry::Entry;
use super::start::{Span, StartKind, StartSpans};

/// A rendered datagram plus where every field landed in it.
pub(crate) struct Rendered {
    pub bytes: Vec<u8>,
    pub start: StartSpans,
    /// `(name, value)` spans, one pair per header line, in wire order.
    pub headers: Vec<(Span, Span)>,
    /// Where the body begins — the end of the header block's blank line.
    pub body_at: usize,
}

/// Render `line`, `entries` and `body` into one buffer, recording spans as it
/// writes.
pub(crate) fn render<S: StartKind>(line: &S::Line, entries: &[Entry], body: &[u8]) -> Rendered {
    write::<S>(line, entries, body, true)
}

/// The datagram alone. A relay forwards bytes and reads no field of what it
/// forwarded, so it records no spans either.
pub(crate) fn render_bytes<S: StartKind>(
    line: &S::Line,
    entries: &[Entry],
    body: &[u8],
) -> Vec<u8> {
    write::<S>(line, entries, body, false).bytes
}

fn write<S: StartKind>(line: &S::Line, entries: &[Entry], body: &[u8], record: bool) -> Rendered {
    const FIRST_LINE_HINT: usize = 96;
    const ENTRY_HINT: usize = 72;
    let capacity = FIRST_LINE_HINT + entries.len() * ENTRY_HINT + body.len() + 4;
    let mut out = Wire::with_capacity(capacity);

    let start = S::render_start(line, &mut out);
    out.str("\r\n");

    let mut headers = if record { Vec::with_capacity(entries.len()) } else { Vec::new() };
    for entry in entries {
        let name_at = out.len();
        out.str(entry.name().as_wire_str());
        let name = (name_at, out.len() - name_at);
        out.str(": ");
        let value_at = out.len();
        entry.render_value(&mut out);
        if record {
            headers.push((name, (value_at, out.len() - value_at)));
        }
        out.str("\r\n");
    }

    out.str("\r\n");
    let body_at = out.len();
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);

    Rendered { bytes, start, headers, body_at }
}
