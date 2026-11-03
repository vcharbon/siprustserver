//! The call record's raw context: what a call keeps of its own SIP traffic.

/// What the call keeps of its own SIP traffic for the record it writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CdrConfig {
    /// The per-leg message-ring cap: the last N messages after the leg's
    /// opening (`call::MessageRing::push`), the rest counted as dropped. `0`
    /// (the default) records nothing. Overridable via `B2BUA_CDR_MESSAGE_RING`.
    pub message_ring: usize,
    /// The header names whose every value each ring entry captures, in this
    /// order; matched casing- and compact-form-insensitively. Empty = the
    /// entries carry no header values. Overridable via
    /// `B2BUA_CDR_CAPTURED_HEADERS` (comma-separated).
    pub captured_headers: Vec<String>,
    /// The exact header names an INVITE final response's entry (2xx-6xx, any
    /// direction) captures instead of `captured_headers`, matched the same
    /// way; `None` = `captured_headers`. Overridable via
    /// `B2BUA_CDR_FINAL_RESPONSE_HEADERS` (comma-separated).
    pub final_response_headers: Option<Vec<String>>,
}
