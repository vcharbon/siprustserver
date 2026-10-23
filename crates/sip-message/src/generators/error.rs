//! Why a generator built no message.

use crate::draft::IncompleteDraft;

/// Why a generator built no message.
#[derive(Debug, Clone)]
pub enum GenerateError {
    /// An input the recipe needs was not given: a defect of the caller.
    Missing(&'static str),
    /// The dialog's local CSeq is the largest one: no next request can be
    /// numbered (RFC 3261 §12.2.1.1).
    SequenceExhausted,
    /// The message does not freeze: a line echoed from a received message, or
    /// one the caller stated, does not read as the header it names.
    Unfreezable(IncompleteDraft),
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerateError::Missing(input) => write!(f, "generator input missing: {input}"),
            GenerateError::SequenceExhausted => write!(f, "no CSeq follows the dialog's last one"),
            GenerateError::Unfreezable(e) => write!(f, "generated message does not freeze: {e}"),
        }
    }
}

impl std::error::Error for GenerateError {}

impl From<IncompleteDraft> for GenerateError {
    fn from(e: IncompleteDraft) -> Self {
        GenerateError::Unfreezable(e)
    }
}
