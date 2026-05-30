//! Helpers for constructing `message/sipfrag` payloads (RFC 3420). Port of
//! `src/sip/SipFragUtils.ts`.
//!
//! Used by the REFER subscription NOTIFY sequence to echo the current state of
//! the transferred-to call back to the referrer.

/// Build a sipfrag body containing only a SIP status line (RFC 3515 §2.4.4):
/// `SIP/2.0 <code> <reason>\r\n` as UTF-8 bytes, with the single trailing CRLF
/// RFC 3420 §2.1 requires. No further MIME encoding is added — callers place
/// this verbatim in the NOTIFY body.
pub fn sipfrag_from_status(code: u16, reason: &str) -> Vec<u8> {
    format!("SIP/2.0 {code} {reason}\r\n").into_bytes()
}
