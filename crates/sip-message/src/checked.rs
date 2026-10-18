//! Checked access to a shared byte buffer: the one place a [`Bytes`] is cut
//! by range, the range checked against the buffer first (ADR-0051 §2).

use std::ops::Range;

use bytes::Bytes;

/// `range` of `buf` as a buffer sharing its bytes, or `None` when the range is
/// not inside it.
pub(crate) fn slice(buf: &Bytes, range: Range<usize>) -> Option<Bytes> {
    if range.start > range.end || range.end > buf.len() {
        return None;
    }
    #[expect(clippy::disallowed_methods, reason = "the range is checked inside `buf` above")]
    let part = buf.slice(range);
    Some(part)
}
