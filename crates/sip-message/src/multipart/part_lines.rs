//! Whether a part's lines can be framed whole: its `Content-Type` value and
//! each of its header lines, a `token` name and a value holding no CR, LF or
//! NUL (RFC 2045 §3, ADR-0053).

use super::MultipartPart;
use crate::write_check::{self, WriteRefusal};

/// The first line of `part` that cannot be written whole: its `Content-Type`
/// value, or one of its header lines.
pub(super) fn part_refusal(part: &MultipartPart) -> Option<WriteRefusal> {
    write_check::header_line("Content-Type", &part.content_type)
        .and_then(|()| {
            part.headers.iter().try_for_each(|(name, value)| write_check::header_line(name, value))
        })
        .err()
}

/// `part` with every header line that cannot be written whole left out, or
/// `None` when its `Content-Type` cannot be; each refusal lands in `refused`.
pub(super) fn writable(
    mut part: MultipartPart,
    refused: &mut Vec<WriteRefusal>,
) -> Option<MultipartPart> {
    if let Err(refusal) = write_check::header_line("Content-Type", &part.content_type) {
        refused.push(refusal);
        return None;
    }
    part.headers.retain(|(name, value)| match write_check::header_line(name, value) {
        Ok(()) => true,
        Err(refusal) => {
            refused.push(refusal);
            false
        }
    });
    Some(part)
}
