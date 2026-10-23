//! The **settle contract** (`PCAP2TEST_PIVOT_V3.md` §10).
//!
//! After the last flow node the runner ALWAYS runs a settle phase: it waits
//! until every scripted node has completed, the system reports no active call,
//! and the CDR expectation is met, bounded by `timing.settle_budget_ms`.
//!
//! **Failing to settle is always test failure. There is no soft mode.** A run
//! that reached its last step and left a call up has not passed; a run whose
//! budget expired states what was still open when it did.
//!
//! The scripted legs' own transactions are the interpreter's half of "every
//! scripted dialog is terminal" ([`floor`]): a non-2xx INVITE final a leg sent
//! holds a server transaction in Completed until the ACK it is owed or Timer H
//! (RFC 3261 §17.2.1), and an INVITE a leg sent holds a client transaction
//! until its final (§17.1.1.2) — the run is not settled while either is open.

use std::collections::BTreeMap;

use pivot_schema::bundle::Failure;
use pivot_schema::postcondition::{CdrExpectation, Postconditions};
use sip_retransmit::timers::{TIMER_B, TIMER_H};

use crate::checks::{self, Observables};
use crate::close::{self, PendingInvite, UnackedFinal};
use crate::recording::Recording;
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

/// A non-2xx INVITE final a scripted leg sent and the system has not
/// acknowledged: the leg, and the transaction as its ladder states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitedAck {
    pub leg: String,
    pub owed: UnackedFinal,
}

impl std::fmt::Display for AwaitedAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let UnackedFinal { cseq, status, .. } = self.owed;
        write!(f, "leg {}: {status} to INVITE CSeq {cseq} awaits its ACK", self.leg)
    }
}

/// An INVITE a scripted leg sent that no final has answered: the leg, and the
/// client transaction as its ladder states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitedFinal {
    pub leg: String,
    pub invite: PendingInvite,
}

impl std::fmt::Display for AwaitedFinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "leg {}: INVITE CSeq {} awaits its final", self.leg, self.invite.cseq)
    }
}

/// The settle floor the scripted legs' own INVITE transactions hold (RFC 3261
/// §17.1.1, §17.2.1). `held` (inside Timer H from the final's first emission)
/// and `awaiting` (Proceeding, or Calling inside Timer B from the INVITE's)
/// keep the run open. `expired` and `unanswered` are past those timers, the
/// transaction gone — the system's failure, holding nothing further.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Floor {
    pub held: Vec<AwaitedAck>,
    pub expired: Vec<AwaitedAck>,
    pub awaiting: Vec<AwaitedFinal>,
    pub unanswered: Vec<AwaitedFinal>,
}

impl Floor {
    /// Whether the floor keeps the run open.
    pub fn holds(&self) -> bool {
        !self.held.is_empty() || !self.awaiting.is_empty()
    }

    /// What the floor holds open, in the settle's own words.
    pub fn reasons(&self) -> Vec<String> {
        let acks = self.held.iter().map(ToString::to_string);
        acks.chain(self.awaiting.iter().map(ToString::to_string)).collect()
    }
}

/// The floor at `now_us`, read off every leg the run recorded — the same
/// ladder the generic close reads, so an emission and what answers it are
/// matched where they were recorded, whoever composed them.
///
/// Timer B bounds only an INVITE in Calling (§17.1.1.2): once a provisional
/// has arrived, nothing but its final or the settle budget ends the wait.
pub fn floor(recording: &Recording, now_us: u64) -> Floor {
    let timer_h_us = TIMER_H * 1000;
    let timer_b_us = TIMER_B * 1000;
    let mut floor = Floor::default();
    for (leg, messages) in recording.legs() {
        for owed in close::unacked_finals(&messages) {
            let sent_at_us = owed.sent_at_us;
            let awaited = AwaitedAck { leg: leg.clone(), owed };
            if now_us.saturating_sub(sent_at_us) < timer_h_us {
                floor.held.push(awaited);
            } else {
                floor.expired.push(awaited);
            }
        }
        for invite in close::pending_invites(&messages) {
            let calling_past_b =
                !invite.proceeding && now_us.saturating_sub(invite.sent_at_us) >= timer_b_us;
            let awaited = AwaitedFinal { leg: leg.clone(), invite };
            if calling_past_b {
                floor.unanswered.push(awaited);
            } else {
                floor.awaiting.push(awaited);
            }
        }
    }
    floor
}

/// What was still open when the settle budget ran out.
pub fn open_reasons(
    flow_done: bool,
    sut: &dyn Sut,
    postconditions: Option<&Postconditions>,
) -> Vec<String> {
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
/// because `{ count, checks }` has no per-record scoping. The
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
mod tests;
