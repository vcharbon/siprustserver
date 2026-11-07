//! The bound on what a party may send at the hop it first reaches (ADR-0001). A
//! refusal happens here, once, where it can be answered: whatever a later hop
//! adds reads with room.

use crate::error::SipParseError;
use crate::types::SipHeader;

/// What a party's new request, one with no To-tag, may hold at first ingress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstIngressBound {
    /// Max bytes of a single header (`name + ": " + value`, unfolded).
    pub header_length: usize,
    /// Max bytes of the Request-URI: a Contact that fits its line fits it, so
    /// the Contact can become the Request-URI of this stack's request.
    pub uri_length: usize,
    /// Max lines of the header block, the start line and folded continuation
    /// lines included.
    pub header_lines: usize,
}

impl FirstIngressBound {
    /// The bound ADR-0001 states.
    pub const DEFAULT: FirstIngressBound =
        FirstIngressBound { header_length: 8192, uri_length: 8192, header_lines: 100 };

    /// The refusal a request of `lines` lines, Request-URI length `uri_length`
    /// and header lines `headers` draws, if any.
    pub(super) fn refusal<'a>(
        &self,
        lines: usize,
        uri_length: usize,
        mut headers: impl Iterator<Item = &'a SipHeader>,
    ) -> Option<SipParseError> {
        if lines > self.header_lines {
            return Some(SipParseError::new(format!(
                "header block of {lines} lines exceeds the first-ingress bound {}",
                self.header_lines
            )));
        }
        if uri_length > self.uri_length {
            return Some(SipParseError::new(format!(
                "Request-URI length {uri_length} exceeds the first-ingress bound {}",
                self.uri_length
            )));
        }
        let long = headers.find(|h| h.name.len() + 2 + h.value.len() > self.header_length)?;
        Some(SipParseError::new(format!(
            "Header {} length {} exceeds the first-ingress bound {}",
            long.name,
            long.name.len() + 2 + long.value.len(),
            self.header_length
        )))
    }
}
