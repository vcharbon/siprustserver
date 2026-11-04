//! [`RuleDiagnostic`]: an input a rule could not read.

/// An input a rule could not read, stated as a fact instead of absorbed into a
/// fabricated value. A rule that hits one names the field and why, emits
/// whatever refusal it owns (a reject response, a terminated subscription), and
/// lets the engine log and count the diagnostic — the alternative a rule surface
/// without this seam is forced into is choosing between silence and acting as if
/// the input were fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDiagnostic {
    /// The input that did not read (`refer_to`, `destination.port`, …).
    pub field: String,
    /// Why it did not read.
    pub reason: String,
}

impl RuleDiagnostic {
    pub fn unreadable(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self { field: field.into(), reason: reason.into() }
    }
}

impl std::fmt::Display for RuleDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unreadable {}: {}", self.field, self.reason)
    }
}
