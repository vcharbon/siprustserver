//! The **call-records claim** (`postconditions.cdr`, or
//! `postconditions.claims["call-records"]`): what a run states of it, and how
//! the records the system wrote answer it.
//!
//! The run never waits for a record. Once it has run, it states the claim
//! ([`state`]): each check's value with its accessors resolved against the run,
//! and whether the check gates on this lane (§9.1). Whoever later holds the
//! records written for the run's calls answers it ([`judge`]): the count
//! exactly, each check over the record SET.

use std::collections::BTreeMap;

use pivot_schema::bundle::{Failure, Informative, RunVerdict};
use pivot_schema::check::{Check, CheckOp};
use pivot_schema::postcondition::CdrCheck;
use serde::{Deserialize, Serialize};

use crate::checks::{self, Observables};
use crate::resolve::Resolver;
use crate::scope::Scope;

pub use pivot_schema::postcondition::CALL_RECORDS;

/// The call-records claim as one run states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordsClaim {
    /// How many records the run's calls must have written, exactly.
    pub count: u32,
    /// Field assertions over the record set, each value resolved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<StatedCheck>,
}

/// One field assertion of the claim, its value carrying no accessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatedCheck {
    #[serde(flatten)]
    pub check: Check,
    /// Whether a record set failing it fails the run on this lane; a check
    /// that does not gate carries its class and is recorded as informative.
    pub gates: bool,
    /// Why the check's value did not resolve against the run, its value then
    /// the one written: no record set satisfies the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
}

/// The claim `expected` states for a run that resolves accessors with
/// `resolver` on the lane `scope` binds. A check whose value does not resolve
/// stays in the claim, failing whatever the records ([`StatedCheck::unresolved`]).
pub fn state(expected: &CdrCheck, resolver: &Resolver<'_>, scope: &Scope<'_>) -> RecordsClaim {
    let checks = expected
        .checks
        .iter()
        .map(|check| {
            let resolved = match &check.value {
                None => Ok(None),
                Some(text) if check.op == CheckOp::Regex => resolver.pattern(text).map(Some),
                Some(text) => resolver.text(text).map(Some),
            };
            let gates = scope.gates(check.class);
            match resolved {
                Ok(value) => {
                    StatedCheck { check: Check { value, ..check.clone() }, gates, unresolved: None }
                }
                Err(e) => {
                    StatedCheck { check: check.clone(), gates, unresolved: Some(e.to_string()) }
                }
            }
        })
        .collect();
    RecordsClaim { count: expected.count, checks }
}

/// Answers `claim` over `records`, each a record as its field map, into
/// `verdict`: a count other than the claimed one fails it, and so does a
/// gating check no record satisfies; a check that does not gate is recorded
/// as informative. Answers whether the claim held. Each check's value is the
/// stated one, compared as written: it is never resolved again. A check whose
/// value did not resolve fails the same way, saying why.
///
/// A check asserts over the record SET — "some record shows this" — because
/// `{ count, checks }` has no per-record scoping, and its failure says so.
pub fn judge(
    claim: &RecordsClaim,
    records: &[BTreeMap<String, String>],
    verdict: &mut RunVerdict,
) -> bool {
    let mut held = true;
    if records.len() as u32 != claim.count {
        held = false;
        verdict.fail(Failure::CdrMismatch {
            expected: format!("{} record(s)", claim.count),
            observed: describe(records),
        });
    }
    for stated in &claim.checks {
        let check = &stated.check;
        let satisfied = stated.unresolved.is_none()
            && records.iter().any(|record| {
                checks::evaluate_resolved("cdr", check, &RecordObservables(record)).is_none()
            });
        if satisfied {
            continue;
        }
        let observed = match &stated.unresolved {
            Some(why) => format!("the check's value does not resolve against the run: {why}"),
            None => describe(records),
        };
        let finding = Failure::CdrMismatch {
            expected: format!(
                "some record where {} {:?} {}",
                check.field,
                check.value.as_deref().unwrap_or(""),
                format_args!("({:?})", check.op)
            ),
            observed,
        };
        match (stated.gates, check.class) {
            (false, Some(class)) => verdict.informative.push(Informative { class, finding }),
            _ => {
                held = false;
                verdict.fail(finding);
            }
        }
    }
    held
}

/// The records a run wrote, with the field a reader has to see to act on the
/// failure. Bounded: a record is a handful of fields, and the whole point is
/// that the verdict is diagnosable without a rerun.
fn describe(records: &[BTreeMap<String, String>]) -> String {
    if records.is_empty() {
        return "no record was written".to_string();
    }
    let rendered: Vec<String> = records
        .iter()
        .map(|record| {
            let fields: Vec<String> =
                record.iter().map(|(field, value)| format!("{field}={value:?}")).collect();
            format!("{{{}}}", fields.join(", "))
        })
        .collect();
    format!("{} record(s): {}", records.len(), rendered.join(" | "))
}

/// One record, as a check source.
struct RecordObservables<'a>(&'a BTreeMap<String, String>);

impl Observables for RecordObservables<'_> {
    fn observe(&self, field: &str) -> Result<Option<String>, String> {
        Ok(self.0.get(field).cloned())
    }
}

#[cfg(test)]
mod tests;
