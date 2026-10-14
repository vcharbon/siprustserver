//! The **post-run RFC audit**, as `rfc.json` in the run bundle: what the
//! lane's recording fabric found when the full RFC suite ran over the run's
//! wire, or the stated fact that no fabric recorded it.
//!
//! The run tests the system under test: a gating finding is the SUT's, and it
//! fails the cell. A document actor's violation is named as that actor's and
//! never fails the cell as the SUT's (§11.1): the document's statement of the
//! same violation on the same transaction cancels it (written beside it, for
//! information); one the document does not state is a SOURCE FAULT — the
//! scripted party replays what the source capture's endpoint did, and that
//! endpoint broke the RFC — which fails the cell as such; one a
//! document that never read its captured trace's violations cannot judge is
//! `unjudged`, for information. An advisory finding is written so a triage
//! session can read it, never counted. `not-audited` is a value of its own, so
//! a lane that recorded nothing can never be read as a clean audit.

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
    /// `true` ⇒ the finding fails the cell as the system under test's:
    /// non-advisory, unwaived, charged to no document actor, and not cancelled
    /// (`cancelled_by` absent).
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
    /// The document actor `charged` names, when it is a document endpoint: the
    /// actor playing the leg the offending call rides (several may share an
    /// endpoint), or the endpoint id where no leg of the run carries it. The
    /// violation is then the scripted peer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// What cancels an actor's finding, where something does: the document's
    /// statement of the same violation on the same transaction — an
    /// `rfc_violations` entry, or a `suppress-auto` deviation withholding the
    /// message the finding misses — named so a reader sees both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_by: Option<String>,
    /// The scripted party whose violation the system under test relayed
    /// onward, where the document states both that party committing it and
    /// the captured platform relaying it, and the run shows that party
    /// committing it: the finding is the SUT's, caused by that party, and
    /// `cancelled_by` names the statements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<String>,
    /// `true` ⇒ a document actor's hard-gate violation the document does not
    /// state: the scripted party replays the source capture's endpoint, which
    /// broke the rule, and the document never says so. Fails the cell as a
    /// source fault, never as the system under test's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_fault: bool,
    /// Why a document actor's uncancelled hard-gate violation is judged
    /// neither cancelled nor a source fault: the document states that it
    /// never read its captured trace's violations. Information only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unjudged: Option<String>,
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
    /// The findings that fail the cell as the system under test's.
    pub fn gating(&self) -> usize {
        self.count(|f| f.gating)
    }

    /// The findings that fail the cell as a source fault.
    pub fn source_faults(&self) -> usize {
        self.count(|f| f.source_fault)
    }

    /// The run passes the audit: audited with no gating finding and no
    /// source fault, or not audited at all (an absence the lane states, never
    /// a pass it claims).
    pub fn passed(&self) -> bool {
        self.gating() == 0 && self.source_faults() == 0
    }

    fn count(&self, admits: impl Fn(&RfcFinding) -> bool) -> usize {
        match self {
            RunRfcAudit::NotAudited { .. } => 0,
            RunRfcAudit::Audited { findings } => findings.iter().filter(|f| admits(f)).count(),
        }
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
            cancelled_by: None,
            caused_by: None,
            source_fault: false,
            unjudged: None,
        }
    }

    /// A scripted party's unstated violation fails the audit without counting
    /// as the system under test's; its two marks are written only where set.
    #[test]
    fn a_source_fault_fails_the_audit_and_is_not_the_systems() {
        let fault = RfcFinding { actor: Some("uas1".into()), source_fault: true, ..finding(false) };
        let audit = RunRfcAudit::Audited { findings: vec![fault] };
        assert_eq!((audit.gating(), audit.source_faults()), (0, 1));
        assert!(!audit.passed());
        let text = serde_json::to_string(&audit).unwrap();
        assert!(text.contains(r#""source_fault":true"#), "{text}");
        assert!(!text.contains("unjudged"), "{text}");
        assert_eq!(serde_json::from_str::<RunRfcAudit>(&text).unwrap(), audit);
        let clean = serde_json::to_string(&finding(true)).unwrap();
        assert!(!clean.contains("source_fault"), "{clean}");
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
