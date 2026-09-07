//! The **post-run RFC audit**, as `rfc.json` in the run bundle: what the
//! lane's recording fabric found when the full RFC suite ran over the run's
//! wire, or the stated fact that no fabric recorded it.
//!
//! A gating finding fails the cell — the same mandatory gate the e2e lane
//! rides, narrowed to what the SUT side EMITTED: a document actor's own
//! deviation is the capture's, replayed as scripted, and is written but never
//! gates. An advisory finding is written so a triage session can read it,
//! never counted. `not-audited` is a value of its own, so a lane that recorded
//! nothing can never be read as a clean audit.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One RFC-suite finding over the run's recorded wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RfcFinding {
    /// The rule id (e.g. `cseq-in-dialog-order`).
    pub rule: String,
    /// The bind (lane) the finding is attributed to.
    pub lane: String,
    /// The rule's own explanation.
    pub detail: String,
    /// `true` ⇒ informational only, never gating.
    pub advisory: bool,
    /// `true` ⇒ the finding fails the cell: non-advisory, unwaived, and not a
    /// document actor's own deviation (`actor` absent).
    pub gating: bool,
    /// The 1-based audit wire-entry index of the offending message, where the
    /// rule pinpoints one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offending: Option<usize>,
    /// The socket the rule holds RESPONSIBLE: the one that emitted the
    /// offending message, or owed the one never emitted. `lane` is where the
    /// finding was reported, which for a taker-vantage rule is the other party.
    /// Absent ⇒ the rule names no culprit, and the finding is gated as the
    /// SUT's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charged: Option<String>,
    /// The document endpoint `charged` is, when it is one: the deviation is
    /// then the scripted peer's, replayed from the capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// What the post-run audit came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "status", rename_all = "kebab-case")]
pub enum RunRfcAudit {
    /// No recording fabric carried the run, so the suite had no input.
    NotAudited {
        /// Why: the lane's own words.
        reason: String,
    },
    /// The full suite ran over the recorded wire; every finding, advisory
    /// included.
    Audited { findings: Vec<RfcFinding> },
}

impl RunRfcAudit {
    /// The findings that fail the cell.
    pub fn gating(&self) -> usize {
        match self {
            RunRfcAudit::NotAudited { .. } => 0,
            RunRfcAudit::Audited { findings } => findings.iter().filter(|f| f.gating).count(),
        }
    }

    /// The run passes the audit: audited with no gating finding, or not
    /// audited at all (an absence the lane states, never a pass it claims).
    pub fn passed(&self) -> bool {
        self.gating() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(gating: bool) -> RfcFinding {
        RfcFinding {
            rule: "cseq-in-dialog-order".into(),
            lane: "b2bua".into(),
            detail: "OPTIONS cseq 1 does not advance".into(),
            advisory: !gating,
            gating,
            offending: Some(7),
            charged: Some("127.0.0.1:5080".into()),
            actor: None,
        }
    }

    #[test]
    fn an_advisory_finding_is_written_and_never_counted() {
        let audit = RunRfcAudit::Audited { findings: vec![finding(false), finding(true)] };
        assert_eq!(audit.gating(), 1);
        assert!(!audit.passed());
        let text = serde_json::to_string(&audit).unwrap();
        assert!(text.contains(r#""status":"audited""#), "{text}");
        assert_eq!(serde_json::from_str::<RunRfcAudit>(&text).unwrap(), audit);
    }

    #[test]
    fn not_audited_is_its_own_value_and_gates_nothing() {
        let audit = RunRfcAudit::NotAudited { reason: "no recording fabric".into() };
        assert!(audit.passed());
        let text = serde_json::to_string(&audit).unwrap();
        assert_eq!(text, r#"{"status":"not-audited","reason":"no recording fabric"}"#);
        assert_eq!(serde_json::from_str::<RunRfcAudit>(&text).unwrap(), audit);
    }

    #[test]
    fn an_unknown_audit_field_is_refused_rather_than_ignored() {
        let text = r#"{"status":"audited","findings":[],"count":0}"#;
        assert!(serde_json::from_str::<RunRfcAudit>(text).is_err());
    }
}
