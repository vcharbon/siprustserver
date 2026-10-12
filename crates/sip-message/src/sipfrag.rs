//! `message/sipfrag` payloads (RFC 3420): built for the REFER subscription
//! NOTIFY sequence, which echoes the current state of the transferred-to call
//! back to the referrer (port of `src/sip/SipFragUtils.ts`), and read back for
//! the header fields a fragment carries.

use crate::{CustomParser, SipHeader};

/// Build a sipfrag body containing only a SIP status line (RFC 3515 §2.4.4):
/// `SIP/2.0 <code> <reason>\r\n` as UTF-8 bytes, with the single trailing CRLF
/// RFC 3420 §2.1 requires. No further MIME encoding is added — callers place
/// this verbatim in the NOTIFY body.
pub fn sipfrag_from_status(code: u16, reason: &str) -> Vec<u8> {
    format!("SIP/2.0 {code} {reason}\r\n").into_bytes()
}

/// The header fields of a fragment (RFC 3420 §2: an optional start line, then
/// header fields, then an optional blank line and body), read as the parser
/// reads a message's header block. A fragment without a start line is read
/// behind a status line of its own, and one without the closing blank line as
/// if it had one; a fragment the parser refuses yields none.
pub fn header_fields(fragment: &[u8]) -> Vec<SipHeader> {
    let first = fragment.split(|b| *b == b'\n').next().unwrap_or_default();
    let first = first.strip_suffix(b"\r").unwrap_or(first);
    let mut image = Vec::with_capacity(fragment.len() + 20);
    if !(first.starts_with(b"SIP/") || first.ends_with(b" SIP/2.0")) {
        image.extend_from_slice(b"SIP/2.0 200 OK\r\n");
    }
    image.extend_from_slice(fragment);
    let closed =
        image.windows(4).any(|w| w == b"\r\n\r\n") || image.windows(2).any(|w| w == b"\n\n");
    if !closed {
        if !image.ends_with(b"\n") {
            image.extend_from_slice(b"\r\n");
        }
        image.extend_from_slice(b"\r\n");
    }
    CustomParser::new().parse_header_fields(&image).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(fragment: &[u8]) -> Vec<(String, String)> {
        header_fields(fragment)
            .into_iter()
            .map(|h| (h.name.as_str().to_string(), h.value.as_str().to_string()))
            .collect()
    }

    #[test]
    fn a_fragment_yields_its_header_fields_with_or_without_a_start_line() {
        let with = b"SIP/2.0 200 OK\r\nFrom: \"Alice\" <sip:alice@example.com>;tag=1\r\n";
        assert_eq!(
            fields(with),
            [("From".to_string(), "\"Alice\" <sip:alice@example.com>;tag=1".to_string())]
        );
        let without = b"To: <sip:bob@example.com>\r\nSubject: hi\r\n\r\n";
        assert_eq!(fields(without).len(), 2);
        assert!(fields(&sipfrag_from_status(180, "Ringing")).is_empty());
    }
}
