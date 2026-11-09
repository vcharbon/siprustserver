//! The parser DI seam.
//!
//! `SipParser` is the layer interface: consumers depend on the trait, not a
//! concrete parser, and a test swaps in a decorator (a parse counter, a
//! fault) behind it. The parity oracle (module `rvoip`, feature
//! `rvoip-oracle`) is no impl: it reads a datagram into the fields a
//! differential test compares.
//!
//! Pure + synchronous + never-panics across the boundary: parse failures are
//! `Err(SipParseError)`, never panics.

use bytes::Bytes;

use crate::error::SipParseError;
use crate::types::SipMessage;
use std::borrow::Cow;

pub mod custom;
mod exemptions;
mod first_ingress;
mod stored;
mod tool_reading;

#[cfg(feature = "rvoip-oracle")]
pub mod rvoip;

pub use exemptions::{Exemption, Exemptions};
pub use first_ingress::FirstIngressBound;
pub(crate) use stored::lenient_header_fields;
pub use stored::read_stored;
pub use tool_reading::ToolReading;

/// The layer interface. Every parser implementation conforms to this.
pub trait SipParser {
    /// Name for benchmark reporting and the compliance matrix.
    fn name(&self) -> &str;

    /// Parse raw wire bytes into a `SipMessage`. Never panics; failures are
    /// returned as `Err`.
    fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError>;

    /// Parse a datagram the caller already owns as [`Bytes`]. The message's
    /// `raw` and `body` then SHARE that buffer instead of copying it — the
    /// receive path should prefer this. Defaults to a copy via [`parse`](Self::parse).
    fn parse_shared(&self, raw: Bytes) -> Result<SipMessage, SipParseError> {
        self.parse(&raw)
    }
}

/// Length caps and grammar policy bounding adversarial input. Lengths are
/// measured against decoded (unfolded, trimmed) values. The `max_*` fields bound
/// every read of the wire; [`first_ingress`](Self::first_ingress) is the smaller
/// bound on a party's new request (ADR-0001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipParserLimits {
    /// Max bytes of a single header (`name + ": " + value`, unfolded).
    pub max_header_length: usize,
    /// Max bytes of the Request-URI in the start line.
    pub max_uri_length: usize,
    /// Max lines of the header block, the start line and folded continuation
    /// lines included. It bounds the work and memory one parse can take; a
    /// message past it is refused before any header is read.
    pub max_header_lines: usize,
    /// Case-insensitive transport allowlist of the topmost Via, the hop this
    /// element answers. Fail-closed by default.
    pub allowed_transports: Cow<'static, [Cow<'static, str>]>,
    /// When `true` (default) the parse holds wire bytes to the RFC 3261 §25.1
    /// grammar of every element it reads and to the gates of ADR-0001. Set
    /// `false` for a message the stack built or a harness or rule validator
    /// reads (ADR-0053 §6).
    pub wire_grammar: bool,
    /// When `true` (default) a value meets the gates a stack-built message and a
    /// harness reading are held to as well: numeric values, quoted-strings,
    /// Digest credentials, one tag on a From or To, a To-tag on a response past
    /// 100. Only [`read_stored`] sets `false`: a stored image met them on its
    /// way in, or holds what the stack stated. A control byte, a Content-Length
    /// that does not frame the message and a second line of a single-valued
    /// header (`header::SINGLE_VALUED`) are refused in every reading.
    pub value_gates: bool,
    /// How the bytes handed to `parse` were delimited, which decides the body
    /// of a message that states no `Content-Length` (RFC 3261 §18.3).
    pub framing: Framing,
    /// The shapes a deployment accepts against the RFC. Held only by an ingress
    /// parser and consulted only by its gates (ADR-0001): a stored read and a
    /// harness reader carry none.
    pub exemptions: Exemptions,
    /// What a party's new request may hold at the hop it first reaches.
    pub first_ingress: FirstIngressBound,
    /// Whether every datagram this parser reads is at first ingress: it refuses
    /// a new request past [`first_ingress`](Self::first_ingress), with the
    /// partial that answers it. An element that cannot tell by the datagram asks
    /// [`CustomParser::first_ingress_refusal`](custom::CustomParser::first_ingress_refusal).
    pub at_first_ingress: bool,
}

/// The transport framing of the bytes a parser is handed. Over a datagram the
/// message ends with the datagram, so a missing `Content-Length` makes the
/// body the remainder (RFC 3261 §18.3); over a stream nothing else delimits
/// the message, so the header is mandatory and its absence is refused
/// (§20.14, §18.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// UDP and every other message-oriented transport.
    #[default]
    Datagram,
    /// TCP, TLS, SCTP and every other stream transport.
    Stream,
}

/// The transports every SIP stack runs over (RFC 3261 §18, RFC 4168, RFC 7118).
const STANDARD_TRANSPORTS: &[Cow<'static, str>] = &[
    Cow::Borrowed("UDP"),
    Cow::Borrowed("TCP"),
    Cow::Borrowed("TLS"),
    Cow::Borrowed("SCTP"),
    Cow::Borrowed("WS"),
    Cow::Borrowed("WSS"),
];

/// The lines every hop of a request's path may add past
/// [`FirstIngressBound::header_lines`]: a forward's Via, double Record-Route and
/// Max-Forwards, a worker's preloaded Route, what a route states.
const HOP_ROOM_LINES: usize = 32;

/// The largest UDP datagram: past first ingress, nothing else bounds a line.
const DATAGRAM_BYTES: usize = 65_535;

impl SipParserLimits {
    /// The default bounds (ADR-0001), built without allocating.
    pub const DEFAULT: SipParserLimits = SipParserLimits {
        max_header_length: DATAGRAM_BYTES,
        max_uri_length: DATAGRAM_BYTES,
        max_header_lines: FirstIngressBound::DEFAULT.header_lines + HOP_ROOM_LINES,
        allowed_transports: Cow::Borrowed(STANDARD_TRANSPORTS),
        wire_grammar: true,
        value_gates: true,
        framing: Framing::Datagram,
        exemptions: Exemptions::NONE,
        first_ingress: FirstIngressBound::DEFAULT,
        at_first_ingress: false,
    };
}
