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
use std::collections::BTreeSet;

pub mod custom;

#[cfg(feature = "rvoip-oracle")]
pub mod rvoip;

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
    pub allowed_transports: BTreeSet<String>,
    /// When `true` (default) the parse holds wire bytes to the RFC 3261 §25.1
    /// grammar of every element it reads and to the gates of ADR-0001. Set
    /// `false` for a message the stack built or a harness or rule validator
    /// reads (ADR-0053 §6). The header split's gates — numeric values, a
    /// repeated Content-Length or Max-Forwards, control bytes, quoted-strings,
    /// Digest credentials — hold in both.
    pub wire_grammar: bool,
    /// How the bytes handed to `parse` were delimited, which decides the body
    /// of a message that states no `Content-Length` (RFC 3261 §18.3).
    pub framing: Framing,
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

impl Default for SipParserLimits {
    fn default() -> Self {
        Self {
            max_header_length: 2048,
            max_uri_length: 2048,
            max_header_lines: 100,
            allowed_transports: ["UDP", "TCP", "TLS", "SCTP", "WS", "WSS"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            wire_grammar: true,
            framing: Framing::Datagram,
        }
    }
}
