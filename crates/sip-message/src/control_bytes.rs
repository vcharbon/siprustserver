//! Where a control byte may stand in a header field (RFC 3261 §25.1).
//!
//! A control byte (CTL, %x00-1F / %x7F) other than HTAB, which is LWS, belongs
//! in one element of the grammar only: the escaped byte of a `quoted-pair`,
//! inside a `quoted-string` or a `comment`. CR and LF are never one: a
//! quoted-pair excludes them, and a header line ends at its CRLF.

use crate::header::HeaderName;

/// Whether `b` is a control byte other than HTAB.
pub(crate) fn is_control(b: u8) -> bool {
    (b < 0x20 && b != b'\t') || b == 0x7f
}

/// Whether the grammar of `name` has a `comment`: a product list (Server,
/// User-Agent) or the delta-seconds comment of Retry-After.
pub(crate) fn has_comments(name: &HeaderName) -> bool {
    matches!(name, HeaderName::Server | HeaderName::UserAgent | HeaderName::RetryAfter)
}

/// Where the scan stands in a value.
#[derive(Clone, Copy)]
enum At {
    Plain,
    Quoted,
    /// Inside a comment, at this nesting depth (one or more).
    Comment(u32),
}

/// The offset in `value` of its first control byte that is no quoted-pair's
/// escaped byte; `None` when every control byte is one. `comments` says
/// whether a `(` opens a comment, as it does in the headers
/// [`has_comments`] names; elsewhere it is a plain byte.
pub(crate) fn bare_control_byte(value: &[u8], comments: bool) -> Option<usize> {
    let mut at = At::Plain;
    let mut bytes = value.iter().copied().enumerate();
    while let Some((i, b)) = bytes.next() {
        at = match at {
            At::Quoted | At::Comment(_) if b == b'\\' => {
                match bytes.next() {
                    Some((escaped_at, escaped)) if escaped == b'\r' || escaped == b'\n' => {
                        return Some(escaped_at)
                    }
                    _ => {}
                }
                at
            }
            _ if is_control(b) => return Some(i),
            At::Plain if b == b'"' => At::Quoted,
            At::Plain if b == b'(' && comments => At::Comment(1),
            At::Quoted if b == b'"' => At::Plain,
            At::Comment(depth) if b == b'(' => At::Comment(depth.saturating_add(1)),
            At::Comment(1) if b == b')' => At::Plain,
            At::Comment(depth) if b == b')' => At::Comment(depth.saturating_sub(1)),
            other => other,
        };
    }
    None
}
