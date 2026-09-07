//! The **settle contract** (`PCAP2TEST_PIVOT_V3.md` §10).
//!
//! After the last flow node the runner ALWAYS runs a settle phase: it waits
//! until every scripted node has completed, the system reports no active call,
//! and the CDR expectation is met, bounded by `timing.settle_budget_ms`.
//!
//! **Failing to settle is always test failure. There is no soft mode.** A run
//! that reached its last step and left a call up has not passed; a run whose
//! budget expired states what was still open when it did.

use std::collections::BTreeMap;

use pivot_schema::bundle::Failure;
use pivot_schema::postcondition::{CdrExpectation, Postconditions};

use crate::checks::{self, Observables};
use crate::resolve::Resolver;
use crate::scope::Finding;

/// What the interpreter needs of the system under test to settle against it.
/// Deployment-neutral: a lane implements it over whatever it runs.
pub trait Sut {
    /// How many calls the system reports as active. Settling waits for zero.
    fn active_calls(&self) -> usize;
    /// The CDRs the run produced, each as its field map. A lane with no CDR
    /// oracle returns none, and a document that asserts one then fails loudly
    /// instead of passing blind.
    fn cdr_records(&self) -> Vec<BTreeMap<String, String>>;
    /// A deployment observable — a metric, a store fact — by name. `None` where
    /// the observable does not exist; `Err` where the name is one this system
    /// does not recognize, which is not the same answer.
    fn observe(&self, name: &str) -> Result<Option<String>, String>;
}

/// The records a run wrote, with the field a reader has to see to act on the
/// failure. Bounded: a CDR is a handful of fields, and the whole point is that
/// the verdict is diagnosable without a rerun.
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

/// One CDR record, as a check source.
struct RecordObservables<'a>(&'a BTreeMap<String, String>);

impl Observables for RecordObservables<'_> {
    fn observe(&self, field: &str) -> Result<Option<String>, String> {
        Ok(self.0.get(field).cloned())
    }
}

/// The system's own observables, as a check source.
struct SutObservables<'a>(&'a dyn Sut);

impl Observables for SutObservables<'_> {
    fn observe(&self, field: &str) -> Result<Option<String>, String> {
        self.0.observe(field)
    }
}

/// Whether the run has reached a settled state. `flow_done` is the
/// interpreter's own half — every scripted node completed — and the rest is the
/// system's.
pub fn is_settled(flow_done: bool, sut: &dyn Sut, postconditions: Option<&Postconditions>) -> bool {
    if !flow_done || sut.active_calls() != 0 {
        return false;
    }
    match postconditions.and_then(|p| p.cdr.as_ref()) {
        Some(CdrExpectation::Expected(cdr)) => sut.cdr_records().len() as u32 >= cdr.count,
        // An absence is reasoned in the document; nothing to wait for.
        Some(CdrExpectation::Absent(_)) | None => true,
    }
}

/// What was still open when the settle budget ran out.
pub fn open_reasons(flow_done: bool, sut: &dyn Sut, postconditions: Option<&Postconditions>) -> Vec<String> {
    let mut open = Vec::new();
    if !flow_done {
        open.push("the flow has not completed".into());
    }
    let active = sut.active_calls();
    if active != 0 {
        open.push(format!("the system reports {active} active call(s)"));
    }
    if let Some(CdrExpectation::Expected(cdr)) = postconditions.and_then(|p| p.cdr.as_ref()) {
        let observed = sut.cdr_records().len();
        if (observed as u32) < cdr.count {
            open.push(format!("{observed} of {} CDR(s) written", cdr.count));
        }
    }
    open
}

/// Evaluate the postconditions once the run has settled: the CDR expectation
/// exactly, then the deployment observables.
///
/// A CDR `checks` entry asserts over the record SET — "some record shows this" —
/// because `{ count, checks }` has no per-record scoping (friction K8). The
/// failure says so rather than implying it pinned one record.
///
/// Each finding carries its check's CLASS; what that costs on this lane is the
/// run's decision (§9.1), taken where the finding is recorded. The record COUNT
/// is unclassified — how many calls a system billed is not a vocabulary.
pub fn evaluate(
    sut: &dyn Sut,
    postconditions: Option<&Postconditions>,
    resolver: &Resolver<'_>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(post) = postconditions else { return out };
    if let Some(CdrExpectation::Expected(cdr)) = &post.cdr {
        let records = sut.cdr_records();
        if records.len() as u32 != cdr.count {
            out.push(Finding::gating(Failure::CdrMismatch {
                expected: format!("{} record(s)", cdr.count),
                observed: describe(&records),
            }));
        }
        for check in &cdr.checks {
            let held = records.iter().any(|record| {
                checks::evaluate("cdr", check, &RecordObservables(record), resolver).is_none()
            });
            if !held {
                out.push(Finding::new(
                    check.class,
                    Failure::CdrMismatch {
                        expected: format!(
                            "some record where {} {:?} {}",
                            check.field,
                            check.value.as_deref().unwrap_or(""),
                            format_args!("({:?})", check.op)
                        ),
                        // A count without the records is a failure nobody can
                        // act on: what the run DID bill is the evidence.
                        observed: describe(&records),
                    },
                ));
            }
        }
    }
    for check in &post.checks {
        if let Some(failure) =
            checks::evaluate("postcondition", check, &SutObservables(sut), resolver)
        {
            out.push(Finding::new(check.class, failure));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RunState;
    use pivot_schema::bundle::IdentityBindings;
    use pivot_schema::scoping::CheckClass;

    struct FakeSut {
        active: usize,
        cdrs: Vec<BTreeMap<String, String>>,
        metrics: BTreeMap<String, String>,
    }

    impl Sut for FakeSut {
        fn active_calls(&self) -> usize {
            self.active
        }
        fn cdr_records(&self) -> Vec<BTreeMap<String, String>> {
            self.cdrs.clone()
        }
        fn observe(&self, name: &str) -> Result<Option<String>, String> {
            if name.starts_with("sip_") {
                Ok(self.metrics.get(name).cloned())
            } else {
                Err(format!("{name:?} is not a metric this system publishes"))
            }
        }
    }

    fn record(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn post(text: &str) -> Postconditions {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn settling_waits_on_the_flow_the_call_count_and_the_cdr_count() {
        let expectation = post(r#"{"cdr":{"count":1}}"#);
        let mut sut = FakeSut { active: 1, cdrs: vec![], metrics: BTreeMap::new() };
        assert!(!is_settled(true, &sut, Some(&expectation)));
        assert!(!is_settled(false, &sut, Some(&expectation)));
        sut.active = 0;
        assert!(!is_settled(true, &sut, Some(&expectation)), "the CDR has not been written");
        let reasons = open_reasons(false, &sut, Some(&expectation));
        assert_eq!(reasons, ["the flow has not completed", "0 of 1 CDR(s) written"]);
        sut.cdrs.push(record(&[("disposition", "ANSWERED")]));
        assert!(is_settled(true, &sut, Some(&expectation)));
        assert!(open_reasons(true, &sut, Some(&expectation)).is_empty());
    }

    #[test]
    fn a_reasoned_cdr_absence_waits_for_nothing_and_asserts_nothing() {
        let expectation = post(r#"{"cdr":{"absent":"capture-carries-no-cdr"}}"#);
        let sut = FakeSut { active: 0, cdrs: vec![], metrics: BTreeMap::new() };
        assert!(is_settled(true, &sut, Some(&expectation)));
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        assert!(evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings)).is_empty());
    }

    #[test]
    fn the_cdr_count_is_exact_at_evaluation_even_though_settling_waits_for_at_least() {
        let expectation = post(r#"{"cdr":{"count":1}}"#);
        let sut = FakeSut {
            active: 0,
            cdrs: vec![record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])],
            metrics: BTreeMap::new(),
        };
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let failures = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
        assert_eq!(failures[0].class, None, "a record COUNT is not a vocabulary");
        let Failure::CdrMismatch { expected, observed } = &failures[0].failure else {
            panic!("a CDR mismatch: {failures:?}")
        };
        assert_eq!(expected, "1 record(s)");
        assert!(observed.starts_with("2 record(s):"), "{observed}");
        assert!(observed.contains("disposition=\"ANSWERED\""), "{observed}");
    }

    #[test]
    fn a_cdr_check_asserts_over_the_record_set_and_says_so_when_none_matches() {
        let expectation = post(
            r#"{"cdr":{"count":2,"checks":[{"field":"disposition","op":"regex","value":"^(ANSWERED|CANCELLED)$"}]}}"#,
        );
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let resolver = Resolver::new(&state, &bindings);
        let matching = FakeSut {
            active: 0,
            cdrs: vec![record(&[("disposition", "ANSWERED")]), record(&[("disposition", "BUSY")])],
            metrics: BTreeMap::new(),
        };
        assert!(evaluate(&matching, Some(&expectation), &resolver).is_empty());
        let none = FakeSut {
            active: 0,
            cdrs: vec![record(&[("disposition", "BUSY")]), record(&[("disposition", "BUSY")])],
            metrics: BTreeMap::new(),
        };
        let failures = evaluate(&none, Some(&expectation), &resolver);
        assert_eq!(failures.len(), 1, "{failures:?}");
        // The failure carries what was actually billed, not only how many.
        let Failure::CdrMismatch { observed, .. } = &failures[0].failure else {
            panic!("a CDR mismatch")
        };
        assert!(observed.contains("disposition=\"BUSY\""), "{observed}");
    }

    #[test]
    fn a_postcondition_finding_carries_the_class_of_the_check_that_found_it() {
        let expectation = post(
            r#"{"cdr":{"count":1,"checks":[
                {"field":"events","op":"regex","value":"InviteReceived","class":"cdr-vocabulary"},
                {"field":"disposition","op":"eq","value":"ANSWERED"}]},
               "checks":[{"field":"sip_orphans","op":"eq","value":"0","class":"cdr-vocabulary"}]}"#,
        );
        let sut = FakeSut {
            active: 0,
            cdrs: vec![record(&[("events", "invite_received"), ("disposition", "CANCELLED")])],
            metrics: BTreeMap::from([("sip_orphans".to_string(), "3".to_string())]),
        };
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let findings = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
        // Every check is EVALUATED; the class only says what the finding costs.
        let classes: Vec<Option<CheckClass>> = findings.iter().map(|f| f.class).collect();
        assert_eq!(
            classes,
            [Some(CheckClass::CdrVocabulary), None, Some(CheckClass::CdrVocabulary)],
            "{findings:#?}"
        );
    }

    #[test]
    fn a_metric_the_system_does_not_publish_fails_rather_than_reading_as_zero() {
        let expectation =
            post(r#"{"cdr":{"absent":"n/a"},"checks":[{"field":"widgets_total","op":"eq","value":"0"}]}"#);
        let sut = FakeSut { active: 0, cdrs: vec![], metrics: BTreeMap::new() };
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let failures = evaluate(&sut, Some(&expectation), &Resolver::new(&state, &bindings));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            matches!(&failures[0].failure, Failure::CheckFailed { observed, .. } if observed.starts_with("unreadable"))
        );
    }
}
