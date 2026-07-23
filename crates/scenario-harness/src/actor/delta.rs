//! The **accepted-delta policy** seam (ADR-0024 §6): a caller-supplied hook the
//! executor consults when a reception goal's DUE expectation is confronted with
//! a non-matching but classifiable inbound (different request method, different
//! response status) — BEFORE falling back to the mismatch path (auto-react /
//! fail-fast). The upstream crate ships only the hook point, this context and
//! reaction vocabulary, and the mechanics; the registry of which substitutions
//! are blessed under which dialog conditions is caller-side policy code.
//!
//! Every acceptance is recorded as
//! [`ReplayEntry::AcceptedDelta`](super::state::ReplayEntry) — an accepted
//! substitution is never silent.

use std::sync::Arc;

use sip_message::SipRequest;

use super::goals::RequestKind;
use super::state::LegPhase;

/// The caller-supplied policy: given the confrontation context, bless the
/// substitution or not. Stored `Arc<dyn Fn ... + Send + Sync>` exactly like the
/// plan's barrier predicates, so it rides the `Send` plan surfaces unchanged.
pub type AcceptedDeltaPolicy = Arc<dyn Fn(&DeltaContext<'_>) -> DeltaDecision + Send + Sync>;

/// What the policy sees at confrontation time: the due scripted expectation,
/// the observed inbound, and the confronted actor's dialog state.
#[derive(Debug)]
pub struct DeltaContext<'a> {
    /// The confronted actor's role (the leg name).
    pub role: &'static str,
    /// The due scripted expectation — the cursor's next un-fired goal, its
    /// barrier state notwithstanding (an early-arriving substitute can be
    /// accepted before the barrier would have released the goal).
    pub expected: ExpectedStimulus<'a>,
    /// The observed inbound that did not match it.
    pub observed: ObservedStimulus<'a>,
    /// The actor's dialog state at confrontation time.
    pub dialog: DialogSnapshot,
}

/// The scripted expectation side of a confrontation.
#[derive(Debug)]
pub enum ExpectedStimulus<'a> {
    /// An `ExpectRequest` of this kind.
    Request(&'a RequestKind),
    /// An `ExpectResponse` of this status.
    Response { status: u16 },
}

impl ExpectedStimulus<'_> {
    /// The bounded label the `AcceptedDelta` observation records.
    pub(super) fn describe(&self) -> String {
        match self {
            ExpectedStimulus::Request(RequestKind::Initial) => "INVITE".to_string(),
            ExpectedStimulus::Request(RequestKind::InDialog(m)) => m.as_str().to_string(),
            ExpectedStimulus::Request(RequestKind::Cancel) => "CANCEL".to_string(),
            ExpectedStimulus::Response { status } => status.to_string(),
        }
    }
}

/// The observed inbound side of a confrontation.
#[derive(Debug)]
pub enum ObservedStimulus<'a> {
    /// An inbound request — the full message, so a policy can key on headers
    /// or body as well as the method.
    Request(&'a SipRequest),
    /// An inbound response of this status.
    Response { status: u16, reason: &'a str },
}

impl ObservedStimulus<'_> {
    /// The bounded label the `AcceptedDelta` observation records.
    pub(super) fn describe(&self) -> String {
        match self {
            ObservedStimulus::Request(r) => r.method.as_str().to_string(),
            ObservedStimulus::Response { status, .. } => status.to_string(),
        }
    }
}

/// The confronted actor's dialog state, offered so a policy can scope a
/// substitution to a dialog condition (the request's RFC scoping: BYE ≈ CANCEL
/// only with exactly one early dialog — RFC 3261 §15.1.2 vs §9).
#[derive(Debug, Clone, Copy)]
pub struct DialogSnapshot {
    /// Distinct early dialogs on this actor's pending initial INVITE: for a
    /// UAS, the tags it has emitted >100 provisionals under on the CURRENT
    /// initial (the transaction's default tag counts as one); for a caller,
    /// the distinct fork tags OBSERVED on >100 provisionals — not pruned by
    /// per-fork finals, so it can only overcount (fails safe for an `== 1`
    /// scoping). `0` with no pending initial INVITE.
    pub early_dialog_count: usize,
    /// Whether this actor holds a confirmed dialog.
    pub confirmed: bool,
    /// This leg's observed phase.
    pub phase: LegPhase,
}

/// The policy's verdict on one confrontation.
pub enum DeltaDecision {
    /// The substitution is not blessed — the executor proceeds exactly as if
    /// no policy were installed (the mismatch path: auto-react / fail-fast).
    NotAccepted,
    /// The substitution is blessed — the acceptance names its rule, the steps
    /// it satisfies, and the stack's mechanical reaction.
    Accepted(AcceptedDelta),
}

/// One blessed substitution: what the script cursor skips and what the stack
/// does with the observed inbound.
pub struct AcceptedDelta {
    /// The policy's bounded rule name, recorded verbatim in the
    /// `AcceptedDelta` replay entry (the report side keys on it).
    pub rule: &'static str,
    /// How many scripted steps, starting AT the due expectation, the
    /// substitution satisfies — the cursor advances past them un-driven. Must
    /// be >= 1 (the due expectation itself is always the first satisfied step).
    pub satisfies_steps: usize,
    /// The stack's mechanical reaction to the observed inbound.
    pub reaction: DeltaReaction,
}

/// What the stack does mechanically with the accepted inbound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaReaction {
    /// The reactive core's standard RFC-compliant handling of the observed
    /// message: for a request, the reactor's answer table; for a response, the
    /// reactive follow-up already performed. No extra emission beyond it.
    Default,
    /// `200` the observed request's transaction, then answer this actor's
    /// pending initial INVITE `487 Request Terminated` and terminate the leg —
    /// the CANCEL-automatic mechanics (RFC 3261 §9.2 / §15.1.2) riding
    /// whatever request was observed. Valid only for an observed request.
    TerminatePendingInitial,
}
