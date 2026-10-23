//! The datagram parse of a receive loop every call shares, with a parser
//! panic contained to the one datagram it was given (ADR-0051 §5). Sound
//! because the parse holds nothing across the call: it takes owned bytes and
//! returns a fresh message, so no state is left half-written by the unwind.

use std::panic::{catch_unwind, AssertUnwindSafe};

use sip_message::{Bytes, SipMessage, SipParseError};

/// The parser panicked on a datagram: a parser defect. The caller drops the
/// datagram and counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserPanicked;

/// What the parse of one datagram gave: the message, the parser's refusal,
/// or [`ParserPanicked`].
pub type ParseOutcome = Result<Result<SipMessage, SipParseError>, ParserPanicked>;

/// Parse the datagram `raw` with `parse`: the parse's own result, or
/// [`ParserPanicked`] when `parse` panics.
pub fn parse_datagram(
    raw: Bytes,
    parse: impl FnOnce(Bytes) -> Result<SipMessage, SipParseError>,
) -> ParseOutcome {
    catch_unwind(AssertUnwindSafe(|| parse(raw))).map_err(|_payload| ParserPanicked)
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
    fn a_parse_that_panics_is_returned_as_parser_panicked() {
        let parsed = parse_datagram(Bytes::from_static(OPTIONS), |_| panic!("probe: parser panic"));
        assert!(matches!(parsed, Err(ParserPanicked)), "{parsed:?}");
    }

    #[test]
    fn a_parse_that_refuses_returns_its_error() {
        let parser = CustomParser::new();
        let parsed =
            parse_datagram(Bytes::from_static(b"\x00garbage"), |raw| parser.parse_shared(raw));
        assert!(matches!(parsed, Ok(Err(_))), "{parsed:?}");
    }

    #[test]
    fn a_parse_that_succeeds_returns_the_message() {
        let parser = CustomParser::new();
        let parsed = parse_datagram(Bytes::from_static(OPTIONS), |raw| parser.parse_shared(raw));
        assert!(matches!(parsed, Ok(Ok(SipMessage::Request(_)))), "{parsed:?}");
    }
}
