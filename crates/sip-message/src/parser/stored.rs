//! The reading of a message the stack keeps as bytes (ADR-0053 §6): a request
//! it sent, a final it retains. The bytes met ingress or left the stack's own
//! builders, so no limit and no exemption applies, and a node with no setting
//! reads what another node stored.

use std::borrow::Cow;

use super::custom::CustomParser;
use super::exemptions::Exemptions;
use super::{Framing, SipParser, SipParserLimits, STANDARD_TRANSPORTS};
use crate::error::SipParseError;
use crate::types::SipMessage;

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
};

/// The message `image` holds. A refusal is a stack defect or a damaged
/// replication frame, which the caller reports; it never panics.
pub fn read_stored(image: &[u8]) -> Result<SipMessage, SipParseError> {
    CustomParser::with_limits(STORED).parse(image)
}
