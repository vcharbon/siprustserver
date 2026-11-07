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
mod stored;

#[cfg(feature = "rvoip-oracle")]
pub mod rvoip;

pub use exemptions::{Exemption, Exemptions};
pub use stored::read_stored;

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
/// measured against decoded (unfolded, trimmed) values.
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
    /// harness reading are held to as well: numeric values, a repeated
    /// Max-Forwards, quoted-strings, Digest credentials, one tag on a From or
    /// To, a To-tag on a response past 100. Only [`read_stored`] sets `false`:
    /// a stored image met them on its way in, or holds what the stack stated. A
    /// control byte and a Content-Length that does not frame the message are
    /// refused in every reading.
    pub value_gates: bool,
    /// How the bytes handed to `parse` were delimited, which decides the body
    /// of a message that states no `Content-Length` (RFC 3261 §18.3).
    pub framing: Framing,
    /// The shapes a deployment accepts against the RFC. Held only by an ingress
    /// parser and consulted only by its gates (ADR-0001): a stored read and a
    /// harness reader carry none.
    pub exemptions: Exemptions,
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

/// The most header lines one forward through the front proxy adds to a request:
/// its Via and a double Record-Route.
const PROXY_HOP_LINES: usize = 3;

/// The most bytes one forward through the front proxy adds to a line it keeps:
/// `;received=<IPv6 address>;rport=<port>` on the top Via.
const PROXY_HOP_LINE_BYTES: usize = 128;

impl SipParserLimits {
    /// The default bounds (ADR-0001), built without allocating: what the edge
    /// of the stack reads under and a stack-built message is assembled under.
    pub const DEFAULT: SipParserLimits = SipParserLimits {
        max_header_length: 8192,
        max_uri_length: 8192,
        max_header_lines: 100,
        allowed_transports: Cow::Borrowed(STANDARD_TRANSPORTS),
        wire_grammar: true,
        value_gates: true,
        framing: Framing::Datagram,
        exemptions: Exemptions::NONE,
    };

    /// These bounds for an element behind the front proxy: a request the proxy
    /// read under them still reads here, its forward's additions included.
    pub fn behind_proxy(self) -> Self {
        Self {
            max_header_length: self.max_header_length.saturating_add(PROXY_HOP_LINE_BYTES),
            max_header_lines: self.max_header_lines.saturating_add(PROXY_HOP_LINES),
            ..self
        }
    }
}

impl Default for SipParserLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
