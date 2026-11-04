//! The **run verdict**: what the run decided, and — where it failed — enough to
//! diagnose it without re-running.
//!
//! Every failure names its site. An expect failure states the step, the leg,
//! what the step gated on and what arrived instead; a settle failure states what
//! was still open when the budget ran out; a plan failure states every
//! compilation refusal. Nothing is downgraded to a warning to make a run pass,
//! and a run that did not fully settle is a FAILURE, always.
//!
//! A NEGATIVE document (§11.2) is decided here and nowhere else: the failures
//! below are raised exactly as they are on any other run, and the interpreter
//! reads them afterwards against the [`crate::must_fail`] declarations the
//! document states.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::known_bug::KnownBug;
use crate::must_fail::DeclaredFailure;
use crate::scoping::CheckClass;
use crate::violation::RfcRule;

mod abandoned;
mod failure;

pub use abandoned::{Abandoned, CloseAct, CloseOwed};
pub use failure::{Arrived, Failure, GatedOn};

/// The run's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VerdictStatus {
    /// Every step ran, the run settled, and every postcondition held.
    Ok,
    /// A NEGATIVE document's run that failed exactly as it declared (§11.2):
    /// every declared failure observed, and no other failure beside them.
    /// Spelled apart from `ok` so nothing reads a case that passed BY FAILING
    /// as a case that passed.
    #[serde(rename = "ok-negative")]
    OkNegative,
    /// At least one failure. The failures say which.
    Failed,
}

/// One RFC violation the document declares, as the verdict lists it (§11.1).
///
/// A scripted peer's entry states what that party broke in the source: it is
/// listed and gates nothing itself, and the RFC audit cancels the run's finding
/// of the same violation on the same transaction against it (`rfc.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ViolationNote {
    /// The rule broken.
    #[schemars(with = "crate::violation::RuleToken")]
    pub rule: RfcRule,
    /// The flow step whose message breaks it.
    pub step: String,
    /// Who emits it: an actor, or `sut`.
    pub emitter: String,
    /// Whether the run's status turns on it. False for every scripted peer,
    /// and for a `sut` entry that relays one.
    pub gating: bool,
    /// The step of the scripted party's entry a `sut` entry carries on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<String>,
}

/// One `must_fail` declaration, against what the run produced (§11.2).
///
/// The declared failure is listed HERE rather than in `failures`, for the same
/// reason an [`Informative`] finding is: the run's `failures` list is what went
/// wrong, and a failure the document ordered up did not go wrong. It is the
/// same [`Failure`] value the gate raised, moved rather than rewritten, so a
/// reader sees the site and the evidence exactly as an ordinary run states them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct DeclaredNote {
    /// What the document declared the run would produce.
    pub failure: DeclaredFailure,
    /// The anchor the declaration names: the step the divergence turns on.
    pub step: String,
    /// The §11.1 rule the SOURCE broke, whose violation predicted this.
    #[schemars(with = "crate::violation::RuleToken")]
    pub derived_from: RfcRule,
    /// The failure the run actually produced for it, where the gate raised one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<Failure>,
    /// The declared datagram as the RECORDING holds it, where it arrived after
    /// the script had already ended and no gate was armed to refuse it (§11.2).
    /// A declaration is satisfied by either field; `None` in both is the run
    /// having produced none, which fails it and says so in `failures`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<String>,
}

/// One classified check that did not hold and did not gate (§9.1).
///
/// The check IS evaluated — a downgrade is not a skip — and its finding lands
/// here instead of in `failures`, so the run's status stays a statement about
/// gating checks alone and the reader still sees what the other lane's
/// vocabulary would have said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Informative {
    /// Which vocabulary the check reads.
    pub class: CheckClass,
    /// What it found.
    pub finding: Failure,
}

/// One check a lane's declared known bug stood down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Waived {
    /// The lane-declared defect that stood the gate down.
    pub bug: KnownBug,
    /// What the check found, evaluated exactly as a gating one is.
    pub finding: Failure,
}

/// One step's retransmission ladder, as the run counted it (§6.9).
///
/// One step's retransmission ladder, declared against observed (§6.9): a
/// `send`'s ladder is what the peer emitted, an `expect`'s what came back
/// byte-identical to the datagram the step matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LadderSide {
    /// The scripted peer paced this ladder itself, so its count holds by
    /// construction and a difference is the run's own defect.
    Send,
    /// The SUT paced it and this side only counted the arrivals, on its own T1
    /// rather than the one the document's count was read off.
    Expect,
}

/// The count is per STEP, which is where the document states it. Beside the two
/// counts it states the ladder's FACTS, so a reader can see WHICH ladder the run
/// asserted: `intervals_ms` is the pacing the document's count was read under,
/// `dwell_us` the window the closer gave the ladder, `rfc_rungs` what an
/// RFC-paced ladder of this message's class puts inside the window it had —
/// which is what an expect of a paced class is held to (§6.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RetransmitNote {
    pub step: String,
    pub leg: String,
    /// Which side paced the ladder this note counts.
    pub side: LadderSide,
    /// What the document states, beyond the first message.
    pub declared: u32,
    /// What the run saw. Equal to `declared` on a run that held.
    pub observed: u32,
    /// The document's own gaps for this step (§6.9); empty where it stated none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intervals_ms: Vec<u64>,
    /// The claimed datagram to the closer that ended the ladder; absent where
    /// nothing closed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_us: Option<u64>,
    /// The rungs an RFC-paced ladder puts inside the ladder's window — the
    /// dwell, or the run itself where nothing closed it. Absent where the class
    /// rides no timer of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfc_rungs: Option<u32>,
}

/// One timer-anchored dwell, declared against observed (§9.2).
///
/// Every `timer_linked` expect the run completed is listed, whether the run
/// measured the declared value exactly or the tolerance absorbed a difference:
/// a window that hides what it swallowed is a window nobody can audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct TimingNote {
    pub step: String,
    pub leg: String,
    /// What the document states the system's timer measures (§6.8).
    pub declared_ms: u64,
    /// What this run measured between the dwell's anchor and the arrival.
    pub observed_ms: u64,
    /// Observed minus declared: negative fired early, positive fired late.
    pub delta_ms: i64,
    /// The ± window this run stated it would accept.
    pub tolerance_ms: u64,
}

/// The run's verdict, as `verdict.json` in the run bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RunVerdict {
    pub case: String,
    pub lane: String,
    pub status: VerdictStatus,
    /// Every failure, in the order the run found them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    /// The step the run failed at, where one failure names a step. The viewer's
    /// compare view keys on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    /// `alt` node id → the branch that ran, for every alt the run committed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub branches: std::collections::BTreeMap<String, String>,
    /// Flow steps that completed, in completion order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_steps: Vec<String>,
    /// Expect steps released without arriving because they were `optional`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released_optional: Vec<String>,
    /// Required expect steps a final on their own transaction retired: the
    /// status it carried was charged on them, and no other rides that
    /// transaction again (RFC 3261 §17.1.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<String>,
    /// The RFC violations the document declares, each with whether it gates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rfc_violations: Vec<ViolationNote>,
    /// The `must_fail` declarations the document states, each against what the
    /// run produced for it (§11.2). Non-empty exactly on a negative case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_fail: Vec<DeclaredNote>,
    /// Divergences a NEGATIVE case carried without turning red (§11.2): the
    /// wire-class failures that co-occurred with the declared one, at or after
    /// the divergence it declares. They are MOVED here rather than dropped, so
    /// `failures` can be empty on `ok-negative` while a reader still sees
    /// everything else the replay diverged on. Empty on every other run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerated: Vec<Failure>,
    /// The script a run that could not go on ended, and the generic close that
    /// terminated its call instead (§11.2). Absent on every run that followed
    /// its flow to the end, whatever its verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<Abandoned>,
    /// Classified checks that did not hold on a lane they do not gate on. The
    /// status above is computed from `failures` alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub informative: Vec<Informative>,
    /// Gates the LANE stood down for, each naming the known bug it declared
    /// (`RunConfig.known_bugs`). The datagram matched and the run went on; the
    /// status above is computed from `failures` alone. A run that waived
    /// nothing carries none, so a reader can tell a clean match from a match
    /// bought by a waiver.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waived: Vec<Waived>,
    /// The retransmission ladders the document declares, declared against
    /// observed (§6.9). A step that declares none is absent from this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retransmits: Vec<RetransmitNote>,
    /// The timer-anchored dwells the run measured, declared against observed
    /// (§9.2). A document that declares none is absent from this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timings: Vec<TimingNote>,
    /// What the lane states about how it ran the case, by name (which service
    /// answered it, and why), so a sweep can count it. Echoed, never
    /// interpreted; the status above is computed from `failures` alone.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub lane_facts: std::collections::BTreeMap<String, String>,
}

impl RunVerdict {
    /// A verdict for a run that produced no failure.
    pub fn ok(case: impl Into<String>, lane: impl Into<String>) -> Self {
        RunVerdict {
            case: case.into(),
            lane: lane.into(),
            status: VerdictStatus::Ok,
            failures: Vec::new(),
            failed_step: None,
            branches: std::collections::BTreeMap::new(),
            completed_steps: Vec::new(),
            released_optional: Vec::new(),
            retired: Vec::new(),
            rfc_violations: Vec::new(),
            must_fail: Vec::new(),
            tolerated: Vec::new(),
            abandoned: None,
            informative: Vec::new(),
            waived: Vec::new(),
            retransmits: Vec::new(),
            timings: Vec::new(),
            lane_facts: std::collections::BTreeMap::new(),
        }
    }

    /// List a declared violation. A scripted peer's, and the system under
    /// test's relaying one, are recorded and nothing else here — the RFC audit
    /// reads them as cancellations; the system under test's own also fails the
    /// run, because it gates.
    pub fn note_violation(&mut self, violation: &crate::violation::RfcViolation) {
        let gating = violation.gates();
        self.rfc_violations.push(ViolationNote {
            rule: violation.rule,
            step: violation.step.clone(),
            emitter: violation.emitter.clone(),
            gating,
            relays: violation.relays.clone(),
        });
        if gating {
            self.fail(Failure::RfcViolationUnverified {
                rule: violation.rule,
                step: violation.step.clone(),
                emitter: violation.emitter.clone(),
            });
        }
    }

    /// Add a failure; the verdict is failed from the first one, and the first
    /// failure that names a step names the run's.
    pub fn fail(&mut self, failure: Failure) {
        self.status = VerdictStatus::Failed;
        if self.failed_step.is_none() {
            self.failed_step = failure.step().map(str::to_string);
        }
        self.failures.push(failure);
    }

    /// Whether the run PASSED — green, or green-as-negative (§11.2). Both are
    /// outcomes a case can be built on; neither carries an open failure.
    pub fn passed(&self) -> bool {
        matches!(self.status, VerdictStatus::Ok | VerdictStatus::OkNegative)
            && self.failures.is_empty()
    }
}

#[cfg(test)]
mod tests;
