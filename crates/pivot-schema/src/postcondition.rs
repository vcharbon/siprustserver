//! `postconditions` (`PCAP2TEST_PIVOT_V3.md` §10): what must hold once the flow
//! has run and the settle phase has completed.
//!
//! The settle phase itself is not optional and is not stated here: after the
//! last flow step the runner ALWAYS waits until every scripted dialog is
//! terminal and the system reports no active call, bounded by
//! `timing.settle_budget_ms`, and failing to settle is test failure. What this
//! block adds is the case's own evidence: how many CDRs the run must have
//! written, and any deployment observable checked once at settle.
//!
//! **CDR checking is default-on.** A document that states no `cdr` block is
//! refused by lint; a document that genuinely has no CDR oracle says so with a
//! reason token, which makes the gap greppable instead of invisible.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::Check;

/// Assertions evaluated once, after settle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Postconditions {
    /// The call-detail-record expectation. Required (lint), because a call test
    /// that never looks at what was billed is half a test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdr: Option<CdrExpectation>,
    /// Deployment observables — metric names, store contents — checked at
    /// settle and never mid-flow. Open field vocabulary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
}

/// What the run must have billed, or why nothing can be said about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CdrExpectation {
    /// The records the run must have written.
    Expected(CdrCheck),
    /// No CDR assertion, with the reason stated. An open token, so a
    /// deployment names its own gaps; what is closed is that a gap must be
    /// NAMED.
    Absent(CdrAbsent),
}

/// The CDR count the run must produce, plus any field assertions over them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdrCheck {
    /// How many records the run must have written.
    pub count: u32,
    /// Field assertions over them, in the one check vocabulary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
}

/// A stated absence of a CDR assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdrAbsent {
    /// Open reason token, e.g. `capture-carries-no-cdr`.
    pub absent: String,
}

impl CdrExpectation {
    /// The field assertions the expectation carries. A stated absence carries
    /// none, which is what makes it an absence.
    pub fn checks(&self) -> &[Check] {
        match self {
            CdrExpectation::Expected(check) => &check.checks,
            CdrExpectation::Absent(_) => &[],
        }
    }
}

impl Postconditions {
    /// Whether the CDR expectation is stated in either of its two forms, with
    /// a non-empty reason where it is an absence.
    pub fn cdr_is_declared(&self) -> bool {
        match &self.cdr {
            Some(CdrExpectation::Expected(_)) => true,
            Some(CdrExpectation::Absent(a)) => !a.absent.is_empty(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Postconditions {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_cdr_expectation_is_a_count_or_a_named_absence() {
        assert!(parse(r#"{"cdr":{"count":2}}"#).cdr_is_declared());
        assert!(parse(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#).cdr_is_declared());
        assert!(!parse(r#"{"checks":[{"field":"m","op":"exists"}]}"#).cdr_is_declared());
        assert!(!parse(r#"{"cdr":{"absent":""}}"#).cdr_is_declared());
    }

    #[test]
    fn a_count_and_a_reason_cannot_be_stated_at_once() {
        assert!(serde_json::from_str::<Postconditions>(r#"{"cdr":{"count":1,"absent":"x"}}"#).is_err());
        assert!(serde_json::from_str::<Postconditions>(r#"{"cdr":{}}"#).is_err());
    }

    #[test]
    fn a_cdr_block_round_trips_with_its_field_checks() {
        let text = r#"{"cdr":{"count":1,"checks":[{"field":"disposition","op":"eq","value":"ANSWERED"}]}}"#;
        assert_eq!(serde_json::to_string(&parse(text)).unwrap(), text);
    }
}
