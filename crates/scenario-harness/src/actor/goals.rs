//! The **goal cursor** — one endpoint's scripted *intent*, shrunk to an ordered
//! list of goals with barrier guards. The reactive answering is NOT scripted
//! (it lives in [`super::actor::default_react`]); a goal is only the deliberate
//! action an endpoint takes (originate the call, hang up, transfer). A goal may
//! wait on a barrier over the observed state before it fires — and because the
//! reactor runs concurrently in the same `select!`, a goal parked on a barrier
//! never blocks the reactor from answering inbound traffic (the structural fix
//! for the cascade).

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;
use sip_message::{EmitOpts, MessageTemplate};

use super::state::{await_pred, LegPhase, ObservedState, StateInner};
use crate::realcall::InvitePlan;
use crate::StepError;

/// A fork's early-dialog id on a scripted UAS. Used DIRECTLY as the fork's
/// To-tag (RFC 3261 §12.1.2), so a peer's reception goal names the same fork
/// by the tag it observes on the wire. Early ids therefore bind
/// SIMULATED-peer forks only — an SUT-generated fork tag can never equal a
/// frozen id; observed-ordinal binding is follow-up reception-goal surface.
pub type EarlyId = &'static str;

/// What a reception goal requires of the received message's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyExpect {
    /// No body requirement.
    Any,
    /// A non-empty body of any type.
    Present,
    /// A non-empty body whose Content-Type is a session description.
    SdpPresent,
}

/// Which kind of request an [`GoalStep::ExpectRequest`] consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// The dialog-creating INVITE (no To-tag).
    Initial,
    /// An in-dialog request of this method (To-tag present).
    InDialog(InDialogMethod),
}

/// The truncated-variant anchor's assertion over the observed final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalAssert {
    /// Exactly this status.
    Exact(u16),
    /// This status class (`2` accepts any 2xx).
    Class(u16),
    /// Any non-error final (< 400).
    NonError,
}

/// A barrier guard over the observed multi-endpoint state.
#[derive(Clone)]
pub enum Barrier {
    /// Fire immediately (no guard).
    None,
    /// Fire once every named leg is at least `Confirmed` — the `established`
    /// guard (bob's REFER / alice's BYE wait on it).
    AllConfirmed(&'static [&'static str]),
    /// Fire once a named predicate over the observed state holds — the open
    /// form (the refer `merged` conjunction is `Pred`). The name is the bounded
    /// label a guard-timeout `StepError::who` carries (B7: never free-form).
    Pred {
        name: &'static str,
        pred: Arc<dyn Fn(&StateInner) -> bool + Send + Sync>,
    },
}

impl Barrier {
    /// A [`Barrier::Pred`] from a name + predicate (the ergonomic constructor).
    pub fn pred(
        name: &'static str,
        pred: impl Fn(&StateInner) -> bool + Send + Sync + 'static,
    ) -> Self {
        Barrier::Pred { name, pred: Arc::new(pred) }
    }

    /// A barrier that holds once `leg` has received an inbound in-dialog request
    /// of `method` (the RFC name, e.g. `"INFO"`) — the observed-fact gate that
    /// orders an origination AFTER a specific inbound request (an MRF's
    /// `INFO(EOF)` following the worker's `INFO(play)`), replacing a timed
    /// post-confirm dwell. `name` is the bounded barrier label a guard-timeout
    /// [`StepError::who`](crate::StepError) carries (B7: never free-form).
    pub fn received(name: &'static str, leg: &'static str, method: &'static str) -> Self {
        Barrier::pred(name, move |s| s.leg_received_method(leg, method))
    }

    /// Whether the guard holds against a state snapshot.
    pub fn holds(&self, s: &StateInner) -> bool {
        match self {
            Barrier::None => true,
            Barrier::AllConfirmed(roles) => {
                roles.iter().all(|r| s.leg_at_least(r, LegPhase::Confirmed))
            }
            Barrier::Pred { pred, .. } => pred(s),
        }
    }

    /// A bounded label for the barrier-timeout `StepError::who` (never free-form).
    pub fn name(&self) -> &'static str {
        match self {
            Barrier::None => "none",
            Barrier::AllConfirmed(_) => "established",
            Barrier::Pred { name, .. } => name,
        }
    }
}

impl std::fmt::Debug for Barrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Barrier::None => f.write_str("Barrier::None"),
            Barrier::AllConfirmed(roles) => write!(f, "Barrier::AllConfirmed({roles:?})"),
            Barrier::Pred { name, .. } => write!(f, "Barrier::Pred({name:?})"),
        }
    }
}

/// One deliberate action an endpoint drives. `Clone` (not `Copy` — the REFER
/// goal carries owned env-derived strings) so the reactor's `select!` arm can
/// lift it out of the cursor without a borrow escaping the future.
#[derive(Debug, Clone)]
pub enum GoalStep {
    /// Originate the initial INVITE to a callee role (the caller's first goal).
    /// `plan` is the owned realization of `CallEnv::outgoing_invite` (routing
    /// through the SUT, correlation stamp, egress rewrite) — `None` sends
    /// directly to the bound target (the SUT-less toy call).
    Invite { callee: &'static str, plan: Option<InvitePlan> },
    /// Send a REFER on the confirmed dialog (the blind transfer). `refer_to` is
    /// the `Refer-To` value; `authorization` the optional `X-Api-Call` payload
    /// the SUT's REFER backend authorizes. Both are extracted OWNED from the
    /// `CallEnv` at build time.
    Refer { refer_to: String, authorization: Option<String> },
    /// Send a DELAYED-OFFER (bodyless) re-INVITE on the confirmed dialog (the
    /// `reinvite` renegotiation). Opens a `ReInvite` obligation keyed on its
    /// CSeq; the reactor ACKs the 2xx WITH the answer SDP (RFC 3264 §4) and
    /// advances the caller's `reneg` sub-flow.
    Reinvite,
    /// Send an in-dialog UPDATE (RFC 3311) carrying an offer on the confirmed
    /// dialog (the `prack_update` renegotiation). Opens an `Update` obligation;
    /// its 200 closes it (no ACK) and advances the caller's `reneg` sub-flow.
    Update,
    /// Send an EARLY UPDATE (RFC 3311 §5.1) on the caller's EARLY dialog — the
    /// still-pending INVITE's dialog, after its reliable provisional was PRACKed
    /// and BEFORE the final 200 (C5). Renegotiates media pre-answer; the callee
    /// (`ReliableAnswerEarlyUpdate`) answers it 200 and only THEN sends the final
    /// 200 INVITE. Opens an `Update` obligation on the early dialog's CSeq.
    UpdateEarly,
    /// Send ONE in-dialog OPTIONS keepalive ping on the confirmed dialog and
    /// read its 200 inline — the `long_call` single ping. The first ping stamps
    /// the `keepalive_ack` feed.
    Options,
    /// Loop an in-dialog OPTIONS keepalive ping every `cadence` until `hold`
    /// elapses (the `options_hold` keepalive loop) — each 200 read inline; the
    /// first stamps `keepalive_ack`.
    EveryOptions { cadence: Duration, hold: Duration },
    /// CANCEL the still-pending initial INVITE (RFC 3261 §9.1) — the abandon
    /// path. Keeps the pending INVITE so its `487` still routes to it.
    Cancel,
    /// Originate a plain in-dialog request (`INFO`/`MESSAGE`) on the confirmed
    /// dialog, optionally carrying a typed body + extra headers — the GENERIC
    /// origination for the long tail of body-carrying in-dialog requests that
    /// have no dedicated goal. Its **2xx alone** completes it (no ACK, no
    /// sub-flow); it opens an [`ObligationKind::InDialog`](super::ledger::ObligationKind::InDialog)
    /// obligation keyed on its CSeq that the reactor closes on the 2xx, so a
    /// dropped request (or its 2xx) holds the settle barrier open until re-emitted
    /// — the same loss-soak contract every other in-dialog request gets. `Info`
    /// today (the MSCML `INFO(EOF)` an MRF media leg sends to release the caller);
    /// `Message` is the plausible next. `content_type`/`body` ride the request
    /// verbatim (SIP bodies are bytes — MSCML is UTF-8 XML); `headers` are extra
    /// request headers.
    InDialog {
        method: InDialogMethod,
        content_type: Option<String>,
        body: Option<Vec<u8>>,
        headers: Vec<(String, String)>,
    },
    /// Hang up — send a BYE on the confirmed dialog.
    Bye,
    /// Hang up IF the dialog confirmed, else a NO-OP — the branch-conditional
    /// teardown for a race whose two legal outcomes differ in whether a dialog
    /// exists (C2/E5 CANCEL×200: the 200-wins branch has a confirmed dialog to
    /// BYE; the CANCEL-wins branch has none). Gated on a barrier that holds once
    /// the race has RESOLVED (`leg_at_least(<leg>, Confirmed)`, which a
    /// monotone Terminated also satisfies), so it fires exactly once in EITHER
    /// branch and every shape still terminates.
    ByeIfConfirmed,
    /// Hang up with EXTRA request headers on the BYE — the deliberate-deviation
    /// path (e.g. a `Contact` on the BYE, which RFC 3261 §15.1 forbids: the
    /// `bye_with_contact` load-audit-waiver case). Same obligation bookkeeping as
    /// [`GoalStep::Bye`]; the headers ride the outgoing request verbatim.
    ByeWith { headers: Vec<(String, String)> },
    /// Originate the initial INVITE from a captured [`MessageTemplate`] — the
    /// template twin of [`Invite`](Self::Invite): frozen headers/body ride
    /// verbatim, the stack regenerates the tier-1 fields, and `plan`/`via`
    /// route it exactly as a plain `Invite` goal's.
    InviteTemplate {
        callee: &'static str,
        plan: Option<InvitePlan>,
        template: MessageTemplate,
        opts: EmitOpts,
    },
    /// Send an in-dialog (or, with `early`, early-dialog — RFC 3311 §5.1)
    /// request from a template; the method is read from the template. Opens the
    /// method's ledger obligation like its semantic twin would.
    RequestTemplate { template: MessageTemplate, opts: EmitOpts, early: bool },
    /// Answer this actor's BOUND server transaction from a template (status and
    /// reason read from the template). The binding is the transaction the
    /// nearest preceding [`ExpectRequest`](Self::ExpectRequest) consumed, else
    /// the parked initial INVITE. A status < 200 responds WITHOUT consuming the
    /// binding; >= 200 consumes it, with the same dialog/ACK bookkeeping as a
    /// policy answer. `early` names the fork this response belongs to: each
    /// distinct id is a distinct To-tag on the SAME server transaction
    /// (RFC 3261 §12.1.2); the final's id names the winner.
    RespondTemplate { template: MessageTemplate, opts: EmitOpts, early: Option<EarlyId> },
    /// Answer the bound server transaction by POLICY — the completion verb for
    /// flows that stop following a capture. SDP answer from the actor's
    /// `MediaState`; same dialog/ACK bookkeeping as a disposition answer.
    Respond { status: u16 },
    /// Strict reception: the next response on this leg must match. `status <
    /// 200` matches the next provisional of exactly that status (a final
    /// arriving first fails fast); `status >= 200` matches the final. A
    /// different status fails fast (`StepError::WrongStatus`), never by
    /// barrier timeout. `ack_body` overrides the engine-built answer SDP on
    /// the ACK to a delayed-offer re-INVITE 2xx; `early` binds the expectation
    /// to a fork's early dialog; `matcher` verifies headers+body through the
    /// template-match surface.
    ExpectResponse {
        status: u16,
        body: BodyExpect,
        early: Option<EarlyId>,
        ack_body: Option<Vec<u8>>,
        matcher: Option<MessageTemplate>,
    },
    /// Strict reception: consume the next parked request of this kind into the
    /// actor's bound server transaction (the `RespondTemplate`/`Respond` that
    /// follows answers it). `matcher` runs at consume time on the parked
    /// transaction's request.
    ExpectRequest { kind: RequestKind, body: BodyExpect, matcher: Option<MessageTemplate> },
    /// Observed, never asserted: wait for the next final on this leg, record
    /// `(key, expected, observed)` into the replay record, and key the
    /// RFC-compliant follow-up on the OBSERVED status (the reactor's normal
    /// handling — ACK a 2xx, hop-ACK a non-2xx INVITE final).
    ObserveFinal { key: u32, expected: Option<u16> },
    /// Strict with a class-shaped assertion — the truncated-variant anchor.
    /// Follow-up keyed on the observed final like
    /// [`ObserveFinal`](Self::ObserveFinal).
    ExpectFinal { assert: FinalAssert },
}

impl GoalStep {
    /// Whether this is a reception goal (an observation over the shared state).
    /// While the next pending goal is one, the reactor's incidental-failure
    /// shed is suppressed — the goal owns the final's verdict.
    pub(super) fn is_reception(&self) -> bool {
        matches!(
            self,
            GoalStep::ExpectResponse { .. }
                | GoalStep::ExpectRequest { .. }
                | GoalStep::ObserveFinal { .. }
                | GoalStep::ExpectFinal { .. }
        )
    }
}

/// A goal: its barrier guard + an optional post-guard dwell + the step it
/// drives. The dwell (`delay`) reproduces the linear bodies' realistic-timing
/// sleeps (talk before transfer, ring before answer) without blocking the
/// reactor — it rides the goal arm of the actor's `select!`.
#[derive(Debug, Clone)]
pub struct Goal {
    pub guard: Barrier,
    pub delay: Duration,
    pub step: GoalStep,
    /// A per-goal override of the standard bounded guard wait — a
    /// capture-declared tighter bound. `None` uses the actor's step timeout.
    pub deadline: Option<Duration>,
}

impl Goal {
    pub fn new(guard: Barrier, step: GoalStep) -> Self {
        Self { guard, delay: Duration::ZERO, step, deadline: None }
    }

    /// Dwell this long AFTER the guard holds, before driving the step (e.g. the
    /// refer body's `reinvite_gap` talk time before the REFER).
    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Bound this goal's guard wait tighter than the actor's step timeout.
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

/// A sequential cursor over an endpoint's goals — the next un-fired goal is the
/// one the reactor's goal arm waits on.
pub struct GoalCursor {
    goals: Vec<Goal>,
    cursor: usize,
    /// The absolute instant the pending goal's post-guard dwell elapses — set
    /// the FIRST time the guard is seen holding, and kept across re-polls (the
    /// goal arm's future is dropped and re-created every time another `select!`
    /// arm wins; without this anchor an inbound message would restart the dwell).
    ready_at: Option<Instant>,
}

impl GoalCursor {
    pub fn new(goals: Vec<Goal>) -> Self {
        Self { goals, cursor: 0, ready_at: None }
    }

    /// Whether an un-fired goal remains.
    pub fn has_pending(&self) -> bool {
        self.cursor < self.goals.len()
    }

    /// Whether every goal has fired.
    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.goals.len()
    }

    /// The next un-fired goal's step, if any.
    pub(super) fn next_step(&self) -> Option<&GoalStep> {
        self.goals.get(self.cursor).map(|g| &g.step)
    }

    /// The steps of every un-fired goal, in order — the park-or-react match set.
    pub(super) fn remaining_steps(&self) -> impl Iterator<Item = &GoalStep> {
        self.goals[self.cursor.min(self.goals.len())..].iter().map(|g| &g.step)
    }

    /// Resolve once the next pending goal's guard holds (immediately for
    /// [`Barrier::None`]) and its post-guard dwell has elapsed, returning the
    /// step to drive — the guard wait is bounded by `timeout` so a genuinely
    /// stuck guard fails the actor rather than hanging. Parks forever if the
    /// cursor is exhausted (the reactor gates this arm on
    /// [`has_pending`](Self::has_pending), so the park is never actually polled).
    pub async fn next_ready(
        &mut self,
        obs: &ObservedState,
        timeout: Duration,
    ) -> Result<GoalStep, StepError> {
        let Some(goal) = self.goals.get(self.cursor) else {
            return std::future::pending().await;
        };
        // Lift the guard/delay out so the dwell anchor below can borrow `self`
        // mutably (the guard is Arc-backed, the clone is cheap).
        let (guard, delay) = (goal.guard.clone(), goal.delay);
        let deadline = Instant::now() + goal.deadline.unwrap_or(timeout);
        await_pred(obs, guard.name(), |s| guard.holds(s), deadline).await?;
        if !delay.is_zero() {
            let at = *self.ready_at.get_or_insert(Instant::now() + delay);
            tokio::time::sleep_until(at).await;
        }
        Ok(self.goals[self.cursor].step.clone())
    }

    /// Advance past the goal that just fired (resets the dwell anchor for the
    /// next goal).
    pub fn advance(&mut self) {
        self.cursor += 1;
        self.ready_at = None;
    }
}
