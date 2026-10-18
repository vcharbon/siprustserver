//! The datagram parse of a receive loop every call shares, with a parser
//! panic contained to the one datagram it was given (ADR-0051 §5). Sound
//! because the parse holds nothing across the call: it takes owned bytes and
//! returns a fresh message, so no state is left half-written by the unwind.

use std::panic::{catch_unwind, AssertUnwindSafe};

use bytes::Bytes;
use sip_message::{SipMessage, SipParseError};

/// What the parse of one datagram gave.
#[derive(Debug)]
pub enum Parsed {
    /// The message the datagram carries.
    Message(SipMessage),
    /// The parser refused the datagram.
    Malformed(SipParseError),
    /// The parser panicked on the datagram: a parser defect. The caller drops
    /// the datagram and counts it.
    Panicked,
}

/// Parse the datagram `raw` with `parse`; a panic inside `parse` is caught and
/// returned as [`Parsed::Panicked`].
pub fn parse_datagram(
    raw: Bytes,
    parse: impl FnOnce(Bytes) -> Result<SipMessage, SipParseError>,
) -> Parsed {
    match catch_unwind(AssertUnwindSafe(|| parse(raw))) {
        Ok(Ok(message)) => Parsed::Message(message),
        Ok(Err(error)) => Parsed::Malformed(error),
        Err(_payload) => Parsed::Panicked,
    }
}

#[cfg(test)]
mod tests {
    use sip_message::{CustomParser, SipParser};

    use super::*;

    const OPTIONS: &[u8] = b"OPTIONS sip:b2bua@127.0.0.1:5070 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-guard\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
        To: <sip:b2bua@127.0.0.1:5070>\r\n\
        Call-ID: guard@u\r\n\
        CSeq: 1 OPTIONS\r\n\
        Content-Length: 0\r\n\r\n";

    #[test]
    fn a_parse_that_panics_is_returned_as_panicked() {
        let parsed = parse_datagram(Bytes::from_static(OPTIONS), |_| panic!("probe: parser panic"));
        assert!(matches!(parsed, Parsed::Panicked), "{parsed:?}");
    }

    #[test]
    fn a_parse_that_refuses_is_returned_as_malformed() {
        let parser = CustomParser::new();
        let parsed =
            parse_datagram(Bytes::from_static(b"\x00garbage"), |raw| parser.parse_shared(raw));
        assert!(matches!(parsed, Parsed::Malformed(_)), "{parsed:?}");
    }

    #[test]
    fn a_parse_that_succeeds_returns_the_message() {
        let parser = CustomParser::new();
        let parsed = parse_datagram(Bytes::from_static(OPTIONS), |raw| parser.parse_shared(raw));
        assert!(matches!(parsed, Parsed::Message(SipMessage::Request(_))), "{parsed:?}");
    }
}
