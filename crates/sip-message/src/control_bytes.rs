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

/// Which quoting elements a header's grammar has, so where a quoted-pair may
/// stand in its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Escapes {
    /// A `"` opens a quoted-string.
    pub quoted_strings: bool,
    /// A `(` opens a comment.
    pub comments: bool,
}

impl Escapes {
    /// No quoting element: every control byte is bare.
    const NONE: Escapes = Escapes { quoted_strings: false, comments: false };
    const QUOTED: Escapes = Escapes { quoted_strings: true, comments: false };
    const COMMENTS: Escapes = Escapes { quoted_strings: false, comments: true };
    const BOTH: Escapes = Escapes { quoted_strings: true, comments: true };

    /// The quoting elements of the header `name` names. A header the stack does
    /// not know reads as holding quoted-strings: its own RFC may define them,
    /// so a quoted-pair in one is not refused for want of a grammar.
    pub(crate) fn of(name: Option<&HeaderName>) -> Escapes {
        let Some(name) = name else {
            return Escapes::QUOTED;
        };
        match name {
            // Tokens, numbers, Call-IDs, a date and TEXT-UTF8-TRIM: these
            // grammars have neither a quoted-string nor a comment.
            HeaderName::Allow
            | HeaderName::AllowEvents
            | HeaderName::CallId
            | HeaderName::ContentEncoding
            | HeaderName::ContentLanguage
            | HeaderName::ContentLength
            | HeaderName::ContentTransferEncoding
            | HeaderName::CSeq
            | HeaderName::Date
            | HeaderName::Expires
            | HeaderName::InReplyTo
            | HeaderName::MaxForwards
            | HeaderName::MimeVersion
            | HeaderName::MinExpires
            | HeaderName::Organization
            | HeaderName::PEarlyMedia
            | HeaderName::Priority
            | HeaderName::Privacy
            | HeaderName::ProxyRequire
            | HeaderName::RAck
            | HeaderName::Require
            | HeaderName::ResourcePriority
            | HeaderName::RSeq
            | HeaderName::Subject
            | HeaderName::Supported
            | HeaderName::Timestamp
            | HeaderName::Unsupported => Escapes::NONE,
            // A product list: products and comments, no quoted-string.
            HeaderName::Server | HeaderName::UserAgent => Escapes::COMMENTS,
            // `delta-seconds [ comment ] *( SEMI retry-param )`, a
            // generic-param's value possibly quoted.
            HeaderName::RetryAfter => Escapes::BOTH,
            _ => Escapes::QUOTED,
        }
    }
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
/// escaped byte; `None` when every control byte is one. `escapes` says
/// whether a `"` opens a quoted-string and a `(` a comment; where one does
/// not, it is a plain byte.
pub(crate) fn bare_control_byte(value: &[u8], escapes: Escapes) -> Option<usize> {
    // A value that opens no quoted-string and no comment has no quoted-pair:
    // its first control byte is the answer, found without the state machine.
    let opens = match (escapes.quoted_strings, escapes.comments) {
        (true, true) => memchr::memchr2(b'"', b'(', value),
        (true, false) => memchr::memchr(b'"', value),
        (false, true) => memchr::memchr(b'(', value),
        (false, false) => None,
    };
    if opens.is_none() {
        return value.iter().position(|&b| is_control(b));
    }
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
            At::Plain if b == b'"' && escapes.quoted_strings => At::Quoted,
            At::Plain if b == b'(' && escapes.comments => At::Comment(1),
            At::Quoted if b == b'"' => At::Plain,
            At::Comment(depth) if b == b'(' => At::Comment(depth.saturating_add(1)),
            At::Comment(1) if b == b')' => At::Plain,
            At::Comment(depth) if b == b')' => At::Comment(depth.saturating_sub(1)),
            other => other,
        };
    }
    None
}
