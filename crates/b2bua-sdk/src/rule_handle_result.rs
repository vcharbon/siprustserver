//! What a rule emits when it handles an event: its actions and the inputs it
//! refused to read.

use crate::model::RuleAction;
use crate::rule_diagnostic::RuleDiagnostic;

/// What a rule emits when it handles an event.
#[derive(Debug, Clone, Default)]
pub struct RuleHandleResult {
    pub actions: Vec<RuleAction>,
    /// Inputs this rule refused to read. Carried alongside the actions rather
    /// than in place of them: a rule reports the defect **and** emits the
    /// refusal it owns. The engine logs and counts these; they emit no wire
    /// traffic of their own, so a rule can never route on one.
    pub diagnostics: Vec<RuleDiagnostic>,
    /// The call's record does not rebuild the caller's INVITE the rule read.
    pub unrecoverable: bool,
}

impl RuleHandleResult {
    pub fn new(actions: Vec<RuleAction>) -> Self {
        Self { actions, diagnostics: Vec::new(), unrecoverable: false }
    }

    /// The same actions, carrying `diagnostic` — what a rule returns when an
    /// input did not read and the actions are its refusal.
    pub fn with_diagnostic(mut self, diagnostic: RuleDiagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }
}
