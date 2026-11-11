//! `postconditions` (`PCAP2TEST_PIVOT_V3.md` §10): what must hold once the flow
//! has run and the settle phase has completed.
//!
//! The settle phase itself is not optional and is not stated here: after the
//! last flow step the runner ALWAYS waits until every scripted dialog is
//! terminal and the system reports no active call, bounded by
//! `timing.settle_budget_ms`, and failing to settle is test failure. What this
//! block adds is the case's own evidence: its claims on the collectors that
//! answer after the call (how many call records the run must have written, among
//! them), and any deployment observable checked once at settle.
//!
//! **A claim is never waited for.** The run settles on its flow and on the
//! system reporting no active call; a claim is answered after the call, by
//! whatever the lane wires for its collector.
//!
//! **Call-record checking is default-on.** A document that states no record
//! expectation is refused by lint; a document that genuinely has no record
//! oracle says so with a reason token, which makes the gap greppable instead of
//! invisible.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::Check;

/// The collector `postconditions.cdr` claims on: the records the system writes
/// for each call it handled.
pub const CALL_RECORDS: &str = "call-records";

/// Assertions evaluated once, after settle, and claims answered after the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Postconditions {
    /// The call-record expectation, read as the [`CALL_RECORDS`] claim.
    /// Required (lint), here or as that claim, because a call test that never
    /// looks at what was billed is half a test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdr: Option<CdrExpectation>,
    /// What the case expects of each collector, by collector name, opaque to
    /// the runner. A [`CALL_RECORDS`] claim has the shape of [`CdrCheck`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claims: BTreeMap<String, serde_json::Value>,
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

/// Why a document's [`CALL_RECORDS`] claim cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordsClaimError {
    /// `postconditions.cdr` states a count and `claims` names the collector too.
    StatedTwice,
    /// The claim is not a `{ count, checks }`: the decoder's reason.
    Malformed(String),
}

impl std::fmt::Display for RecordsClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordsClaimError::StatedTwice => write!(
                f,
                "the {CALL_RECORDS} claim is stated twice: as `cdr.count` and in `claims`"
            ),
            RecordsClaimError::Malformed(why) => {
                write!(f, "the {CALL_RECORDS} claim is no {{ count, checks }}: {why}")
            }
        }
    }
}

impl std::error::Error for RecordsClaimError {}

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
    /// Whether the record expectation is stated: a [`CALL_RECORDS`] claim, or
    /// `cdr` in either of its two forms with a non-empty reason where it is an
    /// absence.
    pub fn records_are_declared(&self) -> bool {
        if self.claims.contains_key(CALL_RECORDS) {
            return true;
        }
        match &self.cdr {
            Some(CdrExpectation::Expected(_)) => true,
            Some(CdrExpectation::Absent(a)) => !a.absent.is_empty(),
            None => false,
        }
    }

    /// The [`CALL_RECORDS`] claim: the one `claims` names, else a `cdr` that
    /// states a count. `None` where neither does: a stated absence claims
    /// nothing.
    pub fn call_records(&self) -> Result<Option<CdrCheck>, RecordsClaimError> {
        let Some(claim) = self.claims.get(CALL_RECORDS) else {
            return Ok(match &self.cdr {
                Some(CdrExpectation::Expected(cdr)) => Some(cdr.clone()),
                _ => None,
            });
        };
        if matches!(self.cdr, Some(CdrExpectation::Expected(_))) {
            return Err(RecordsClaimError::StatedTwice);
        }
        serde_json::from_value(claim.clone())
            .map(Some)
            .map_err(|e| RecordsClaimError::Malformed(e.to_string()))
    }

    /// Every claim the document states, by collector name: `claims`, and
    /// `cdr` as the [`CALL_RECORDS`] claim where it states a count and
    /// `claims` does not name that collector.
    pub fn stated_claims(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = self.claims.clone();
        if let Some(CdrExpectation::Expected(cdr)) = &self.cdr {
            if !out.contains_key(CALL_RECORDS) {
                let value = serde_json::to_value(cdr).expect("a record expectation serializes");
                out.insert(CALL_RECORDS.to_string(), value);
            }
        }
        out
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
        assert!(parse(r#"{"cdr":{"count":2}}"#).records_are_declared());
        assert!(parse(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#).records_are_declared());
        assert!(!parse(r#"{"checks":[{"field":"m","op":"exists"}]}"#).records_are_declared());
        assert!(!parse(r#"{"cdr":{"absent":""}}"#).records_are_declared());
        assert!(parse(r#"{"claims":{"call-records":{"count":1}}}"#).records_are_declared());
    }

    #[test]
    fn a_count_and_a_reason_cannot_be_stated_at_once() {
        assert!(
            serde_json::from_str::<Postconditions>(r#"{"cdr":{"count":1,"absent":"x"}}"#).is_err()
        );
        assert!(serde_json::from_str::<Postconditions>(r#"{"cdr":{}}"#).is_err());
    }

    #[test]
    fn a_cdr_block_round_trips_with_its_field_checks() {
        let text = r#"{"cdr":{"count":1,"checks":[{"field":"disposition","op":"eq","value":"ANSWERED"}]}}"#;
        assert_eq!(serde_json::to_string(&parse(text)).unwrap(), text);
    }

    /// `cdr.count` is read as the call-records claim, and a claim of that
    /// name in `claims` reads the same; a stated absence claims nothing.
    #[test]
    fn a_cdr_count_and_a_call_records_claim_read_as_one_claim() {
        let counted = parse(r#"{"cdr":{"count":2,"checks":[{"field":"d","op":"exists"}]}}"#);
        let claimed = parse(
            r#"{"cdr":{"absent":"claims"},"claims":{"call-records":{"count":2,"checks":[{"field":"d","op":"exists"}]},"logs":{"lines":3}}}"#,
        );
        assert_eq!(counted.call_records(), claimed.call_records());
        assert_eq!(counted.call_records().unwrap().map(|c| c.count), Some(2));
        assert_eq!(
            counted.stated_claims()[CALL_RECORDS],
            serde_json::json!({"count": 2, "checks": [{"field": "d", "op": "exists"}]})
        );
        assert_eq!(
            claimed.stated_claims().keys().collect::<Vec<_>>(),
            [CALL_RECORDS, "logs"],
            "every other claim is carried as stated"
        );
        let absent = parse(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#);
        assert_eq!(absent.call_records(), Ok(None));
        assert!(absent.stated_claims().is_empty());
    }

    #[test]
    fn a_call_records_claim_stated_twice_or_malformed_is_refused() {
        let twice = parse(r#"{"cdr":{"count":1},"claims":{"call-records":{"count":1}}}"#);
        assert_eq!(twice.call_records(), Err(RecordsClaimError::StatedTwice));
        let malformed = parse(r#"{"claims":{"call-records":{"lines":1}}}"#);
        assert!(matches!(malformed.call_records(), Err(RecordsClaimError::Malformed(_))));
    }

    #[test]
    fn claims_round_trip_and_an_empty_set_is_not_written() {
        let text = r#"{"cdr":{"absent":"claims"},"claims":{"logs":{"lines":3}}}"#;
        assert_eq!(serde_json::to_string(&parse(text)).unwrap(), text);
        let bare = r#"{"cdr":{"count":1}}"#;
        assert_eq!(serde_json::to_string(&parse(bare)).unwrap(), bare);
    }
}
