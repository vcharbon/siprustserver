//! The parser DI seam. Port of `src/sip/parsers/interface.ts` +
//! `src/sip/Parser.ts`.
//!
//! `SipParser` is the **layer interface** (the DI seam that mimics Effect
//! Layer): consumers depend on the trait, not a concrete parser, and any
//! impl — `CustomParser` (production) or `RvoipParser` (dev-only oracle) —
//! is swappable behind it. Future contract wrappers (the effect-layer-test
//! 4-wrapper model: property/paranoid/parity/scopedAudit, deferred to the
//! SignalingNetwork slice) will also implement this same trait as decorators.
//!
//! Pure + synchronous + never-panics across the boundary: parse failures are
//! `Err(SipParseError)`, never panics. There is no async / Effect runtime at
//! this layer.

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
    /// receive path should prefer this. Defaults to a copy via [`parse`].
    fn parse_shared(&self, raw: Bytes) -> Result<SipMessage, SipParseError> {
        self.parse(&raw)
    }
}

/// Length caps and grammar policy bounding adversarial input. Port of
/// `SipParserLimits`. Lengths are measured against decoded (unfolded,
/// trimmed) values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipParserLimits {
    /// Max bytes of a single header (`name + ": " + value`, unfolded).
    pub max_header_length: usize,
    /// Max bytes of the Request-URI in the start line.
    pub max_uri_length: usize,
    /// Case-insensitive Via transport allowlist. Fail-closed by default.
    pub allowed_transports: BTreeSet<String>,
    /// When `true` (default) the ADR-0007 strict-grammar gates fire. Set
    /// `false` for harness/rule-validator scenarios that must inspect
    /// malformed-but-parseable messages without the parser pre-rejecting.
    pub wire_grammar: bool,
}

impl Default for SipParserLimits {
    fn default() -> Self {
        Self {
            max_header_length: 2048,
            max_uri_length: 2048,
            allowed_transports: ["UDP", "TCP", "TLS", "SCTP", "WS", "WSS"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            wire_grammar: true,
        }
    }
}
