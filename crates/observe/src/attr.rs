//! Attribute shaping: the size cap, and the lossless payload encoding.
//!
//! A traced call records raw wire bytes; one oversized body must not become an
//! unbounded export payload. Every attribute is capped at [`ATTR_CAP_BYTES`]
//! and a capped attribute is marked so a reader never mistakes a prefix for the
//! whole value.
//!
//! This module is the single home for how a payload becomes span attributes —
//! both the B2BUA and the proxy record through [`crate::CallSpan`], which shapes
//! here. [`shape_body`] never mangles: a SIP message that is entirely valid
//! UTF-8 (the overwhelming majority) is carried as READABLE TEXT, byte for byte,
//! and only the non-UTF-8 remainder of a message that has one is base64-encoded
//! alongside it.

use std::borrow::Cow;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// Maximum bytes any single span attribute carries.
pub const ATTR_CAP_BYTES: usize = 16 * 1024;

/// The span field set alongside a capped attribute.
pub const TRUNCATED_FIELD: &str = "truncated";

/// The value of the `body_encoding` field on an event that carries a binary
/// remainder.
pub const BODY_ENCODING_BASE64: &str = "base64";

/// Cap a text attribute, returning the value and whether it was truncated.
/// Truncation lands on a `char` boundary, so the result is always valid UTF-8.
pub fn cap_str(value: &str) -> (Cow<'_, str>, bool) {
    if value.len() <= ATTR_CAP_BYTES {
        return (Cow::Borrowed(value), false);
    }
    let mut end = ATTR_CAP_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (Cow::Borrowed(&value[..end]), true)
}

/// Cap a raw byte attribute (a wire message), returning the value and whether
/// it was truncated.
pub fn cap_bytes(value: &[u8]) -> (&[u8], bool) {
    if value.len() <= ATTR_CAP_BYTES {
        (value, false)
    } else {
        (&value[..ATTR_CAP_BYTES], true)
    }
}

/// The non-UTF-8 remainder of a payload, carried losslessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTail {
    /// The remainder, base64 (standard alphabet, padded).
    pub base64: String,
    /// Byte offset in the original payload where the readable text ends and
    /// this remainder begins.
    pub split_offset: usize,
}

/// A payload shaped into span attributes with no loss of information.
///
/// `text` is the readable part; `binary`, when present, carries everything from
/// `split_offset` on. With `truncated` false the original wire bytes are exactly
/// `text.as_bytes()` followed by the base64-decoded remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapedBody<'a> {
    /// The valid-UTF-8 prefix — the whole payload for an ordinary SIP message.
    pub text: Cow<'a, str>,
    /// Present only when the payload is not entirely valid UTF-8.
    pub binary: Option<BinaryTail>,
    /// Whether either part hit [`ATTR_CAP_BYTES`].
    pub truncated: bool,
}

/// Shape a raw payload for recording.
///
/// A payload that is entirely valid UTF-8 costs one validity scan and no
/// allocation: the text borrows the caller's bytes. Otherwise the payload is
/// split at the first invalid byte — in SIP practice the start line, the headers
/// and usually most of the body stay readable, and only the binary remainder is
/// encoded. Both parts carry the [`ATTR_CAP_BYTES`] cap.
pub fn shape_body(body: &[u8]) -> ShapedBody<'_> {
    match std::str::from_utf8(body) {
        Ok(text) => {
            let (text, truncated) = cap_str(text);
            ShapedBody { text, binary: None, truncated }
        }
        Err(err) => {
            let split_offset = err.valid_up_to();
            // The prefix is valid by construction: `valid_up_to` is where the
            // scan stopped.
            let prefix = std::str::from_utf8(&body[..split_offset]).unwrap_or_default();
            let (text, text_truncated) = cap_str(prefix);
            let (tail, tail_truncated) = cap_bytes(&body[split_offset..]);
            ShapedBody {
                text,
                binary: Some(BinaryTail { base64: BASE64.encode(tail), split_offset }),
                truncated: text_truncated || tail_truncated,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_at_the_cap_is_untouched() {
        let s = "x".repeat(ATTR_CAP_BYTES);
        let (out, truncated) = cap_str(&s);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(!truncated);
    }

    #[test]
    fn an_oversized_value_is_capped_and_marked() {
        let s = "x".repeat(ATTR_CAP_BYTES + 1);
        let (out, truncated) = cap_str(&s);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(truncated);
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        // A 3-byte char straddling the cap must be dropped whole.
        let mut s = "a".repeat(ATTR_CAP_BYTES - 1);
        s.push('€');
        let (out, truncated) = cap_str(&s);
        assert!(truncated);
        assert_eq!(out.len(), ATTR_CAP_BYTES - 1, "the straddling char is dropped whole");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn raw_bytes_cap_at_the_same_ceiling() {
        let b = vec![0xFFu8; ATTR_CAP_BYTES + 9];
        let (out, truncated) = cap_bytes(&b);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(truncated);
        let (out, truncated) = cap_bytes(&b[..8]);
        assert_eq!(out.len(), 8);
        assert!(!truncated);
    }

    const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\nCall-ID: c1@h\r\n\r\nv=0\r\n";

    /// The wire bytes a reader reconstructs from a shaped payload.
    fn reconstructed(shaped: &ShapedBody<'_>) -> Vec<u8> {
        let mut out = shaped.text.as_bytes().to_vec();
        if let Some(tail) = &shaped.binary {
            assert_eq!(tail.split_offset, out.len(), "the split offset names where the text ends");
            out.extend_from_slice(&BASE64.decode(&tail.base64).expect("valid base64"));
        }
        out
    }

    #[test]
    fn a_text_message_stays_readable_and_carries_no_encoding() {
        let shaped = shape_body(INVITE);
        assert_eq!(shaped.text, std::str::from_utf8(INVITE).unwrap());
        assert!(shaped.binary.is_none(), "a valid-UTF-8 message is never encoded");
        assert!(!shaped.truncated);
        assert!(matches!(shaped.text, Cow::Borrowed(_)), "the pure-text path allocates nothing");
    }

    #[test]
    fn a_binary_body_segment_reconstructs_byte_exact() {
        let mut wire = INVITE.to_vec();
        wire.extend_from_slice(&[0xF0, 0x00, 0xFF, 0xFE, b'z']);
        let shaped = shape_body(&wire);

        let tail = shaped.binary.as_ref().expect("the invalid remainder is encoded");
        assert_eq!(tail.split_offset, INVITE.len(), "the readable headers and SDP are not encoded");
        assert!(shaped.text.starts_with("INVITE sip:bob"), "the readable part stays readable");
        assert!(!shaped.truncated);
        assert_eq!(reconstructed(&shaped), wire, "the wire bytes reconstruct exactly");
    }

    #[test]
    fn a_payload_invalid_from_its_first_byte_encodes_whole_with_an_empty_text() {
        let wire = [0xFFu8, 0xFE, 0xFD];
        let shaped = shape_body(&wire);
        assert_eq!(shaped.text, "");
        assert_eq!(shaped.binary.as_ref().expect("encoded").split_offset, 0);
        assert_eq!(reconstructed(&shaped), wire);
    }

    #[test]
    fn both_parts_carry_the_cap_and_the_marker() {
        let mut wire = vec![b'x'; ATTR_CAP_BYTES + 10];
        wire.push(0xFF);
        wire.extend_from_slice(&vec![0xFEu8; ATTR_CAP_BYTES + 10]);
        let shaped = shape_body(&wire);

        assert!(shaped.truncated, "a capped part is always marked");
        assert_eq!(shaped.text.len(), ATTR_CAP_BYTES);
        let tail = shaped.binary.as_ref().expect("encoded");
        assert_eq!(
            BASE64.decode(&tail.base64).expect("valid base64").len(),
            ATTR_CAP_BYTES,
            "the encoded remainder caps at the same ceiling",
        );
    }

    #[test]
    fn an_empty_payload_shapes_to_empty_text() {
        let shaped = shape_body(b"");
        assert_eq!(shaped.text, "");
        assert!(shaped.binary.is_none());
        assert!(!shaped.truncated);
    }
}
