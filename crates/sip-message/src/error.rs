//! The parser's refusal of a message: why it was refused, and what of a
//! refused request can still be answered.

use std::fmt;

use crate::parser::custom::partial::RequestPartial;

/// A message the parser refused. `reason` names the first rule it broke.
/// `partial` holds what a response to a refused request echoes; it is present
/// only when that response can be built correctly (see [`RequestPartial`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipParseError {
    pub reason: String,
    pub partial: Option<Box<RequestPartial>>,
}

impl SipParseError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), partial: None }
    }

    /// What a response to the refused request echoes; `None` when no response
    /// can answer it.
    pub fn partial(&self) -> Option<&RequestPartial> {
        self.partial.as_deref()
    }
}

impl fmt::Display for SipParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SipParseError: {}", self.reason)
    }
}

impl std::error::Error for SipParseError {}
