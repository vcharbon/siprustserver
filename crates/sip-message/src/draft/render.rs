//! The render pass: a draft becomes one pre-sized buffer, and the frozen
//! message's fields become spans of it.
//!
//! Raw entries are memcpy'd from whatever image they point into; typed entries
//! render straight into the same buffer. Nothing is re-parsed and no value gets
//! its own `String`. Each line is checked as it is written: a header name off
//! `token`, a control byte inside the start line, or one outside a
//! quoted-pair inside a header value (CR and LF always), is the render's
//! refusal (ADR-0053).

use crate::control_bytes::has_comments;
use crate::header::{HeaderName, Wire};
use crate::write_check::{self, Position, WriteRefusal};

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
    /// The first line the render could not write whole; a message carrying
    /// one never freezes.
    pub refusal: Option<WriteRefusal>,
}

/// Render `line`, `entries` and `body` into one buffer, recording spans as it
/// writes.
pub(crate) fn render<S: StartKind>(line: &S::Line, entries: &[Entry], body: &[u8]) -> Rendered {
    write::<S>(line, entries, body, true)
}

/// The datagram alone, or the first line it could not write whole. A relay
/// forwards bytes and reads no field of what it forwarded, so it records no
/// spans either.
pub(crate) fn render_bytes<S: StartKind>(
    line: &S::Line,
    entries: &[Entry],
    body: &[u8],
) -> Result<Vec<u8>, WriteRefusal> {
    let rendered = write::<S>(line, entries, body, false);
    match rendered.refusal {
        Some(refusal) => Err(refusal),
        None => Ok(rendered.bytes),
    }
}

fn write<S: StartKind>(line: &S::Line, entries: &[Entry], body: &[u8], record: bool) -> Rendered {
    const FIRST_LINE_HINT: usize = 96;
    const ENTRY_HINT: usize = 72;
    let capacity = FIRST_LINE_HINT + entries.len() * ENTRY_HINT + body.len() + 4;
    let mut out = Wire::with_capacity(capacity);

    let start = S::render_start(line, &mut out);
    let mut refusal = write_check::rendered_start_line(out.as_bytes())
        .map(|reason| WriteRefusal { position: Position::StartLine, reason });
    out.str("\r\n");

    let mut headers = if record { Vec::with_capacity(entries.len()) } else { Vec::new() };
    for entry in entries {
        let name_at = out.len();
        out.str(entry.wire_name());
        let name = (name_at, out.len() - name_at);
        out.str(": ");
        let value_at = out.len();
        entry.render_value(&mut out);
        if record {
            headers.push((name, (value_at, out.len() - value_at)));
        }
        if refusal.is_none() {
            refusal = line_refusal(entry, out.as_bytes().get(value_at..).unwrap_or_default());
        }
        out.str("\r\n");
    }

    out.str("\r\n");
    let body_at = out.len();
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);

    Rendered { bytes, start, headers, body_at, refusal }
}

/// Why the line `name: value` cannot be written whole: an extension name or a
/// caller's spelling off `token`, or a control byte outside a quoted-pair in
/// the value. A canonical known name is a constant `token`.
fn line_refusal(entry: &Entry, value: &[u8]) -> Option<WriteRefusal> {
    let name = entry.name();
    let spelled = match name {
        HeaderName::Other(text) => Some(text),
        _ => entry.spelling(),
    };
    if let Some(text) = spelled {
        if let Err(refusal) = write_check::header_name(text.as_str()) {
            return Some(refusal);
        }
    }
    write_check::rendered_value(value, has_comments(name)).map(|reason| WriteRefusal {
        position: Position::HeaderValue,
        reason: format!("{}: {reason}", name.as_wire_str()),
    })
}
