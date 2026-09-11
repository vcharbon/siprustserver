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

/// The structural identity of one datagram a failure names. Fields, never
/// prose: the post-run confrontation builds its substitution probes from these,
/// so nothing downstream re-parses a description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Arrived {
    /// A request, with the CSeq it rode.
    Request { method: String, cseq: u32 },
    /// A response, with the transaction it answers.
    Response { status: u16, reason: String, cseq_method: String, cseq: u32 },
    /// Bytes no SIP message parsed out of.
    Unreadable,
}

impl std::fmt::Display for Arrived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arrived::Request { method, cseq } => write!(f, "{method} (CSeq {cseq} {method})"),
            Arrived::Response { status, reason, cseq_method, cseq } => {
                write!(f, "{status} {reason} to {cseq_method} (CSeq {cseq})")
            }
            Arrived::Unreadable => write!(f, "an unreadable message"),
        }
    }
}

/// What a refusing expect gated on: its discriminator, as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GatedOn {
    /// A request, by method.
    Request { method: String },
    /// A response, by status and — where the document states it — the CSeq
    /// method of the transaction it answers.
    Response {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cseq_method: Option<String>,
    },
}

impl std::fmt::Display for GatedOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatedOn::Request { method } => write!(f, "{method}"),
            GatedOn::Response { status, cseq_method: Some(m) } => write!(f, "{status} to {m}"),
            GatedOn::Response { status, cseq_method: None } => write!(f, "{status}"),
        }
    }
}

/// One thing that went wrong. Each variant carries the site and the evidence;
/// there is deliberately no catch-all variant that maps distinct failures onto
/// one bland shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "failure", rename_all = "kebab-case")]
pub enum Failure {
    /// The document does not compile to a plan.
    PlanRefused { detail: String },
    /// An `expect` was not satisfied inside its budget.
    ExpectTimedOut { step: String, leg: String, gated_on: String, within_ms: u64 },
    /// A datagram arrived on the leg that the armed expect does not match, and
    /// no background policy answers it. Neither absorbed nor tolerated (§14
    /// item 4, K2). `reason` is the closest gate's own words for the refusal.
    UnmatchedDatagram {
        step: String,
        leg: String,
        gated_on: GatedOn,
        reason: String,
        arrived: Arrived,
    },
    /// A datagram arrived on a leg with no armed expect at all. `detail` is why
    /// it could be attributed to no leg, where that is the story.
    UnexpectedDatagram {
        leg: String,
        arrived: Arrived,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A datagram arrived during the settle window of a flow that had COMPLETED:
    /// the document scripted nothing more, and something came anyway.
    DatagramAfterFlow { leg: String, arrived: Arrived },
    /// An inline or postcondition check did not hold.
    CheckFailed { site: String, field: String, op: String, expected: String, observed: String },
    /// A `${…}` could not be resolved at the moment it was read.
    AccessorUnresolved { site: String, detail: String },
    /// An emission could not be rendered or sent.
    SendFailed { step: String, leg: String, detail: String },
    /// The run did not reach a settled state inside `timing.settle_budget_ms`.
    SettleTimedOut { budget_ms: u64, open: Vec<String> },
    /// A background policy's settle-time count bound did not hold.
    BackgroundCount { actor: String, method: String, bound: String, observed: u32 },
    /// The CDR expectation did not hold.
    CdrMismatch { expected: String, observed: String },
    /// The flow did not finish: these nodes never completed.
    FlowIncomplete { pending: Vec<String> },
    /// The document names a deviation this interpreter would have to EMIT and
    /// cannot. Parsing it is fine; running it is not, because emitting a
    /// compliant message instead would not reproduce the defect.
    DeviationUnimplemented { deviation: String, kind: String, step: Option<String>, reason: String },
    /// The document holds an `inject` and the lane supplied no injector.
    InjectorMissing { node: String, action: String },
    /// The lane bound no number for an identity the run needs — a claim whose
    /// number set is empty matches nothing, so the run refuses before it dials.
    IdentityUnbound { site: String, identity: String, detail: String },
    /// A socket closed or errored under the run. The transport's own words, so
    /// the cause is not lost behind a settle timeout.
    TransportClosed { actor: String, detail: String },
    /// The recording could not be written — the run's evidence is the run's
    /// product, so losing it fails the run.
    RecordingFailed { detail: String },
    /// The run made no progress and virtual time stopped advancing — a stall the
    /// run reports rather than hanging on. Names what it was waiting for.
    RunStalled { phase: String, waiting_on: Vec<String> },
    /// The run body never returned (a panic, an assertion, the RFC gate). The
    /// bundle is what the run had reached when it unwound.
    RunUnwound { detail: String },
    /// The document states an RFC violation the SYSTEM UNDER TEST emits. Such a
    /// violation gates (§11.1), and no detector decides it yet, so the run
    /// refuses rather than passing a claim nothing verified.
    RfcViolationUnverified { rule: RfcRule, step: String, emitter: String },
    /// A step's retransmission ladder (§6.9) is not the one its emitter owed:
    /// the count is a protocol fact, so it gates on every lane. `declared` is
    /// what the document stated and `expected` what the ladder's pacer owes —
    /// the same number on a scripted send, the RFC's rung count where the SUT
    /// paced it.
    RetransmitCountMismatch {
        step: String,
        leg: String,
        declared: u32,
        expected: u32,
        observed: u32,
    },
    /// A `cseq-override` (§11) on a message whose CSeq the stack does not
    /// choose. Emitting the compliant number instead would not reproduce the
    /// defect, so the run refuses rather than ignoring the entry.
    CseqOverrideRefused { step: String, leg: String, deviation: String, reason: String },
    /// A `verbatim-emission` / `raw-order` step whose emission did NOT carry the
    /// stored block as the document holds it. The property the deviation states
    /// is verified against the composed message, never assumed.
    EmissionNotPreserved { step: String, leg: String, deviation: String, detail: String },
    /// A timer-anchored dwell (§6.8 `timer_linked`) the system did not measure:
    /// the arrival sits further from its anchor than the document declares, by
    /// more than the run's stated tolerance (§9.2).
    TimingOutOfTolerance {
        step: String,
        leg: String,
        declared_ms: u64,
        observed_ms: u64,
        tolerance_ms: u64,
    },
    /// The run configuration directs a call the run does not dial: an id no
    /// `calls[]` entry carries, or a call whose caller leg opens with no INVITE
    /// this vantage sends. The directive would silently reach nothing.
    CallDirectiveUnplaced { call: String, detail: String },
    /// A `send` step declares `retransmits` for a message that retransmits on no
    /// timer of its own. Emitting the repeats at an invented pacing would replay
    /// a ladder the document does not state, so the run refuses before the wire.
    RetransmitLadderRefused { step: String, leg: String, detail: String },
    /// A `must_fail` declaration (§11.2) the run did NOT produce. A negative
    /// case passes only by failing exactly as declared, so reality that stopped
    /// failing is this case failing — which is the whole point of declaring it.
    DeclaredFailureNotProduced { declared: DeclaredFailure, step: String, detail: String },
}

impl Failure {
    /// The flow step this failure names, where it names one. A failure about the
    /// run as a whole — a settle timeout, an unwind — names none.
    pub fn step(&self) -> Option<&str> {
        match self {
            Failure::ExpectTimedOut { step, .. }
            | Failure::UnmatchedDatagram { step, .. }
            | Failure::SendFailed { step, .. }
            | Failure::RetransmitCountMismatch { step, .. }
            | Failure::TimingOutOfTolerance { step, .. }
            | Failure::RetransmitLadderRefused { step, .. }
            | Failure::DeclaredFailureNotProduced { step, .. } => Some(step),
            Failure::DeviationUnimplemented { step, .. } => step.as_deref(),
            _ => None,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::PlanRefused { detail } => write!(f, "plan refused: {detail}"),
            Failure::ExpectTimedOut { step, leg, gated_on, within_ms } => write!(
                f,
                "expect {step:?} on leg {leg}: nothing matched {gated_on} within {within_ms} ms"
            ),
            Failure::UnmatchedDatagram { step, leg, gated_on, reason, arrived } => write!(
                f,
                "expect {step:?} on leg {leg} gated on {gated_on} ({reason}); {arrived} arrived instead"
            ),
            Failure::UnexpectedDatagram { leg, arrived, detail: None } => {
                write!(f, "leg {leg} has no armed expect; {arrived} arrived")
            }
            Failure::UnexpectedDatagram { leg, arrived, detail: Some(detail) } => {
                write!(f, "leg {leg} has no armed expect; {arrived} arrived — {detail}")
            }
            Failure::DatagramAfterFlow { leg, arrived } => {
                write!(f, "leg {leg}: {arrived} arrived after the flow completed")
            }
            Failure::CheckFailed { site, field, op, expected, observed } => write!(
                f,
                "{site}: check {field} {op} {expected:?} — observed {observed:?}"
            ),
            Failure::AccessorUnresolved { site, detail } => write!(f, "{site}: {detail}"),
            Failure::SendFailed { step, leg, detail } => {
                write!(f, "send {step:?} on leg {leg}: {detail}")
            }
            Failure::SettleTimedOut { budget_ms, open } => {
                write!(f, "did not settle within {budget_ms} ms; still open: {}", open.join(", "))
            }
            Failure::BackgroundCount { actor, method, bound, observed } => write!(
                f,
                "actor {actor}: {method} background count {observed} does not satisfy {bound}"
            ),
            Failure::CdrMismatch { expected, observed } => {
                write!(f, "CDR expectation {expected} — observed {observed}")
            }
            Failure::FlowIncomplete { pending } => {
                write!(f, "flow did not complete; pending: {}", pending.join(", "))
            }
            Failure::DeviationUnimplemented { deviation, kind, step, reason } => write!(
                f,
                "deviation {deviation:?} of kind {kind:?} (step {step:?}) cannot run: {reason}"
            ),
            Failure::InjectorMissing { node, action } => {
                write!(f, "inject {node:?} names action {action:?} and the lane supplied no injector")
            }
            Failure::IdentityUnbound { site, identity, detail } => {
                write!(f, "{site}: the lane bound no number for identity {identity:?} ({detail})")
            }
            Failure::TransportClosed { actor, detail } => {
                write!(f, "the socket of actor {actor} failed under the run: {detail}")
            }
            Failure::RecordingFailed { detail } => write!(f, "recording failed: {detail}"),
            Failure::RunStalled { phase, waiting_on } => write!(
                f,
                "the run stalled in {phase} with time no longer advancing; waiting on: {}",
                waiting_on.join(", ")
            ),
            Failure::RunUnwound { detail } => write!(f, "the run did not return: {detail}"),
            Failure::RfcViolationUnverified { rule, step, emitter } => write!(
                f,
                "rfc violation {rule} at step {step:?} is emitted by {emitter:?}, the system under test: it gates, and nothing verifies it"
            ),
            Failure::RetransmitCountMismatch { step, leg, declared, expected, observed } => write!(
                f,
                "step {step:?} on leg {leg} owes {expected} retransmission(s) (the document declares {declared}); the run saw {observed}"
            ),
            Failure::CseqOverrideRefused { step, leg, deviation, reason } => write!(
                f,
                "send {step:?} on leg {leg}: cseq-override {deviation:?} cannot be emitted — {reason}"
            ),
            Failure::EmissionNotPreserved { step, leg, deviation, detail } => write!(
                f,
                "send {step:?} on leg {leg}: deviation {deviation:?} states the stored block rides \
                 as held, and {detail}"
            ),
            Failure::TimingOutOfTolerance {
                step,
                leg,
                declared_ms,
                observed_ms,
                tolerance_ms,
            } => write!(
                f,
                "expect {step:?} on leg {leg} rides a timer the document measures at {declared_ms} ms; \
                 the run observed {observed_ms} ms, outside the stated ±{tolerance_ms} ms"
            ),
            Failure::CallDirectiveUnplaced { call, detail } => write!(
                f,
                "the run configuration directs call {call:?} and this run dials it nowhere: {detail}"
            ),
            Failure::RetransmitLadderRefused { step, leg, detail } => {
                write!(f, "send {step:?} on leg {leg} cannot pace its retransmits: {detail}")
            }
            Failure::DeclaredFailureNotProduced { declared, step, detail } => write!(
                f,
                "this document declares {declared} at step {step:?} and the run did not produce it: {detail}"
            ),
        }
    }
}

/// One RFC violation the document declares, as the verdict lists it (§11.1).
///
/// A scripted peer's violation is the case's own subject matter: it is listed
/// prominently and gates nothing, so a reader of the bundle sees what the run
/// deliberately reproduced without the run turning red for reproducing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ViolationNote {
    /// The rule broken.
    pub rule: RfcRule,
    /// The flow step whose message breaks it.
    pub step: String,
    /// Who emits it: an actor, or `sut`.
    pub emitter: String,
    /// Whether the run's status turns on it. False for every scripted peer.
    pub gating: bool,
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

/// What the generic close put on the wire for one leg (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CloseOwed {
    /// A final response to a request the leg took and never answered.
    Answer,
    /// The ACK a final it took is owed (RFC 3261 §13.2.2.4, §17.1.1.3).
    Ack,
    /// The BYE that ends a dialog the leg opened (RFC 3261 §15).
    Bye,
    /// The CANCEL for an INVITE it sent that has no final (RFC 3261 §9.1).
    Cancel,
}

/// One act of the generic close: what a scripted leg emitted to end what it
/// still held once its script had ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CloseAct {
    pub leg: String,
    /// The obligation it discharged.
    pub owed: CloseOwed,
    /// The start line of the message it put on the wire. The datagram itself is
    /// in the recording, on this leg, noted as the close's.
    pub sent: String,
}

/// A script a run that COULD NOT GO ON ended, and the generic close that
/// terminated its call instead (§11.2). Polarity-free: a positive run that
/// cannot go on is abandoned and closed exactly like a negative one, and its
/// verdict still fails.
///
/// The abandonment is stated rather than hidden: `completed_steps` stays the
/// truth about what ran, this is the truth about what did not, and the acts are
/// what ended the call in the script's place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Abandoned {
    /// The leg the aborting delta landed on, where the abort named one. A run
    /// stopped by something legless — a stall, a closed transport, a flow with
    /// nothing left to wake — abandons with no leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
    /// The flow step it named, where it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The flow nodes the script never ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// What the close emitted, in the order it emitted it. Empty where every
    /// scripted leg already held nothing open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<CloseAct>,
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
            rfc_violations: Vec::new(),
            must_fail: Vec::new(),
            tolerated: Vec::new(),
            abandoned: None,
            informative: Vec::new(),
            waived: Vec::new(),
            retransmits: Vec::new(),
            timings: Vec::new(),
        }
    }

    /// List a declared violation. A scripted peer's is recorded and nothing
    /// else; the system under test's also fails the run, because it gates.
    pub fn note_violation(&mut self, violation: &crate::violation::RfcViolation) {
        let gating = violation.sut_emitted();
        self.rfc_violations.push(ViolationNote {
            rule: violation.rule,
            step: violation.step.clone(),
            emitter: violation.emitter.clone(),
            gating,
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
mod tests {
    use super::*;

    #[test]
    fn the_first_step_naming_failure_becomes_the_run_s_failed_step() {
        let mut verdict = RunVerdict::ok("c", "upstream-fake");
        assert!(verdict.passed());
        verdict.fail(Failure::SettleTimedOut { budget_ms: 32000, open: vec!["B".into()] });
        assert!(!verdict.passed());
        assert_eq!(verdict.failed_step, None);
        verdict.fail(Failure::ExpectTimedOut {
            step: "s7".into(),
            leg: "A".into(),
            gated_on: "486".into(),
            within_ms: 32000,
        });
        assert_eq!(verdict.failed_step.as_deref(), Some("s7"));
        verdict.fail(Failure::SendFailed {
            step: "s9".into(),
            leg: "A".into(),
            detail: "x".into(),
        });
        assert_eq!(verdict.failed_step.as_deref(), Some("s7"), "the FIRST one stands");
        assert_eq!(verdict.failures.len(), 3);
    }

    #[test]
    fn a_verdict_round_trips_through_its_bundle_form() {
        let mut verdict = RunVerdict::ok("authored-cancel-race", "upstream-fake");
        verdict.branches.insert("a1".into(), "cancelled".into());
        verdict.completed_steps.push("s1".into());
        verdict.informative.push(Informative {
            class: CheckClass::CdrVocabulary,
            finding: Failure::CdrMismatch {
                expected: "events matches InviteReceived".into(),
                observed: "invite_received".into(),
            },
        });
        let text = serde_json::to_string(&verdict).unwrap();
        assert_eq!(serde_json::from_str::<RunVerdict>(&text).unwrap(), verdict);
        // The status is a statement about gating checks; an informative finding
        // is not one of them.
        assert!(verdict.passed());
    }

    fn violation(emitter: &str) -> crate::violation::RfcViolation {
        crate::violation::RfcViolation {
            rule: RfcRule::No200AfterCancel,
            step: "s11".into(),
            emitter: emitter.into(),
        }
    }

    #[test]
    fn a_scripted_peer_s_violation_is_listed_and_gates_nothing() {
        let mut verdict = RunVerdict::ok("bc-rc-cancel-xing", "upstream-fake");
        verdict.note_violation(&violation("uas1"));
        assert_eq!(verdict.rfc_violations.len(), 1);
        assert!(!verdict.rfc_violations[0].gating);
        assert!(verdict.passed(), "{:#?}", verdict.failures);
        // And it survives the bundle, because the listing is the point.
        let text = serde_json::to_string(&verdict).unwrap();
        assert!(text.contains("no-200-after-cancel"), "{text}");
    }

    #[test]
    fn a_violation_the_system_under_test_emits_gates() {
        let mut verdict = RunVerdict::ok("bc-rc-cancel-xing", "upstream-fake");
        verdict.note_violation(&violation("sut"));
        assert!(verdict.rfc_violations[0].gating);
        assert!(!verdict.passed());
        assert!(matches!(verdict.failures.first(), Some(Failure::RfcViolationUnverified { .. })));
    }
}
