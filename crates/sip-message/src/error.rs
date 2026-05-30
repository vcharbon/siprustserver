//! Parser error type. Port of `src/sip/parsers/errors.ts`.
//!
//! ADR-0007 deliberately carries **no `kind` discriminator** — the `reason`
//! string carries the rule identity (e.g. "Top Via branch missing magic
//! cookie ..."). The secusiptest/compliance classifier matches on substrings.
//! We keep that shape: a single `reason` field.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipParseError {
    pub reason: String,
}

impl SipParseError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }
}

impl fmt::Display for SipParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SipParseError: {}", self.reason)
    }
}

impl std::error::Error for SipParseError {}
