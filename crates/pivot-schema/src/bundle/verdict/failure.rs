//! The failure vocabulary of the verdict: one variant per site, each carrying
//! the site and the evidence as data, and the datagram identity and gate a
//! datagram failure names.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::must_fail::DeclaredFailure;
use crate::violation::RfcRule;

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
    /// item 4). `reason` is the closest gate's own words for the refusal.
    UnmatchedDatagram {
        step: String,
        leg: String,
        gated_on: GatedOn,
        reason: String,
        arrived: Arrived,
    },
    /// An expect of the `100` to an INVITE the leg sent met that transaction's
    /// FINAL first. An INVITE server transaction sends 100 only when its TU
    /// does not answer within 200 ms (RFC 3261 §17.2.1), so the step is retired
    /// as omitted and the final goes on to the step behind it. `after_ms` is
    /// the final's arrival measured from the INVITE's emission.
    TryingOmitted { step: String, leg: String, arrived: Arrived, after_ms: u64 },
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
    /// A non-2xx final a scripted leg sent to an INVITE drew no ACK inside
    /// Timer H (RFC 3261 §17.2.1): the system owed one on the INVITE's branch
    /// (§17.1.1.3) and its transaction never completed.
    FinalUnacknowledged { leg: String, status: u16, cseq: u32 },
    /// An INVITE a scripted leg sent drew no response at all inside Timer B
    /// (RFC 3261 §17.1.1.2): its client transaction timed out in Calling.
    InviteUnanswered { leg: String, cseq: u32 },
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
    RfcViolationUnverified {
        #[schemars(with = "crate::violation::RuleToken")]
        rule: RfcRule,
        step: String,
        emitter: String,
    },
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
            | Failure::TryingOmitted { step, .. }
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
            Failure::TryingOmitted { step, leg, arrived, after_ms } => write!(
                f,
                "expect {step:?} on leg {leg} gated on the 100 to INVITE; {arrived} arrived first, \
                 {after_ms} ms after the INVITE"
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
            Failure::FinalUnacknowledged { leg, status, cseq } => write!(
                f,
                "leg {leg}: the {status} to INVITE CSeq {cseq} drew no ACK inside Timer H"
            ),
            Failure::InviteUnanswered { leg, cseq } => {
                write!(f, "leg {leg}: INVITE CSeq {cseq} drew no response inside Timer B")
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
