//! The reading of a message the stack keeps as bytes (ADR-0053 §6): a request
//! it sent, a final it retains. The bytes met ingress or left the stack's own
//! builders, so no limit and no exemption applies, and a node with no setting
//! reads what another node stored.

use std::borrow::Cow;

use super::custom::CustomParser;
use super::exemptions::Exemptions;
use super::first_ingress::FirstIngressBound;
use super::{Framing, SipParser, SipParserLimits, STANDARD_TRANSPORTS};
use crate::error::SipParseError;
use crate::types::{SipHeader, SipMessage};

/// The lenient reading with every limit off.
const STORED: SipParserLimits = SipParserLimits {
    max_header_length: usize::MAX,
    max_uri_length: usize::MAX,
    max_header_lines: usize::MAX,
    allowed_transports: Cow::Borrowed(STANDARD_TRANSPORTS),
    wire_grammar: false,
    value_gates: false,
    framing: Framing::Datagram,
    exemptions: Exemptions::NONE,
    first_ingress: FirstIngressBound::DEFAULT,
    at_first_ingress: false,
};

/// The message `image` holds. A refusal is a stack defect or a damaged
/// replication frame, which the caller reports; it never panics.
pub fn read_stored(image: &[u8]) -> Result<SipMessage, SipParseError> {
    CustomParser::stored(STORED).parse(image)
}

/// The start line and header block of `raw` read as [`read_stored`] reads a
/// message's, its header fields returned without the mandatory-field
/// extraction: the lenient reading of a head whose body is judged apart.
pub(crate) fn lenient_header_fields(raw: &[u8]) -> Result<Vec<SipHeader>, SipParseError> {
    CustomParser::stored(STORED).parse_header_fields(raw)
}
