//! [`Reading`] — the two readings of one grammar (ADR-0053 §6).

/// How a reader holds a value to its grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// The RFC 3261 §25.1 grammar, the whole value consumed: the reading of
    /// wire bytes, which refuses anything else.
    Strict,
    /// What can be read, never a refusal: the reading of a message held to no
    /// wire grammar, which the stack built or a harness reads.
    Lenient,
}
