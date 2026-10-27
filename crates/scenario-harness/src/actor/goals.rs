//! The **goal cursor** — one endpoint's scripted *intent*, shrunk to an ordered
//! list of goals with barrier guards. The reactive answering is NOT scripted
//! (it lives in [`super::react::default_react`]); a goal is only the deliberate
//! action an endpoint takes (originate the call, hang up, transfer). A goal may
//! wait on a barrier over the observed state before it fires — and because the
//! reactor runs concurrently in the same `select!`, a goal parked on a barrier
//! never blocks the reactor from answering inbound traffic (the structural fix
//! for the cascade).

use std::time::Duration;

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;
use sip_template::{EmitOpts, MessageTemplate};

use super::barrier::await_pred;
pub use super::barrier::Barrier;
use super::endpoint::ForkWinner;
use super::state::ObservedState;
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
    /// The message MUST carry no body — the delayed-offer shape (RFC 3261
    /// §14.2 / RFC 3264 §5: an offerless INVITE hands the offer to its
    /// recipient), and the discriminator between two same-method messages on
    /// one leg when only one of them carries a session description.
    Absent,
    /// A non-empty body of any type.
    Present,
    /// A non-empty body whose Content-Type is a session description.
    SdpPresent,
}

impl BodyExpect {
    /// Whether a body of `body_len` bytes, a session description or not,
    /// satisfies this claim. The ONE place the vocabulary is interpreted: the
    /// reception goals assert through it and the parked-request pick
    /// (`super::select`) discriminates through it.
    pub fn satisfied_by(self, body_len: usize, body_is_sdp: bool) -> bool {
        match self {
            BodyExpect::Any => true,
            BodyExpect::Absent => body_len == 0,
            BodyExpect::Present => body_len > 0,
            BodyExpect::SdpPresent => body_is_sdp,
        }
    }

    /// The bounded lowercase label a divergence record names this claim by
    /// (see [`super::state::ReplayEntry::BodyExpectMiss`]).
    pub fn label(self) -> &'static str {
        match self {
            BodyExpect::Any => "any",
            BodyExpect::Absent => "absent",
            BodyExpect::Present => "present",
            BodyExpect::SdpPresent => "sdp",
        }
    }
}

/// Which kind of request an [`GoalStep::ExpectRequest`] consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// The dialog-creating INVITE (no To-tag).
    Initial,
    /// An in-dialog request of this method (To-tag present).
    InDialog(InDialogMethod),
    /// The CANCEL targeting this actor's pending initial INVITE (RFC 3261
    /// §9.2). The CANCEL hop's `200` stays a stack automatic; consuming the
    /// expectation does NOT rebind, so a following `487` rides the bound
    /// INVITE transaction. Precedence vs the CANCEL automatic: ADR-0024.
    Cancel,
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
    /// Send `method` on an EARLY dialog, before the INVITE's final: a caller on
    /// its pending INVITE's early dialog, a [`Disposition::Held`](super::Disposition::Held) callee on the
    /// early dialog of `fork` (`None`: its own tag) once that dialog's
    /// reliable provisional is PRACKed. An UPDATE carries this endpoint's
    /// session description as an offer (RFC 3311 §5.1). Opens the method's
    /// obligation, which its 2xx closes.
    EarlyRequest { method: InDialogMethod, fork: Option<&'static str> },
    /// Send the provisionals of a [`Disposition::Held`](super::Disposition::Held) callee's INVITE: one
    /// per tag in `forks` (distinct early dialogs on the one transaction, RFC
    /// 3261 §12.1.2), or one under the transaction's own tag when `forks` is
    /// empty. `reliable` makes each a `183` with `Require: 100rel`, an `RSeq`
    /// and the answer SDP (RFC 3262), else a plain `180`.
    Ring { forks: &'static [&'static str], reliable: bool },
    /// Answer a [`Disposition::Held`](super::Disposition::Held) callee's INVITE `200` + SDP under
    /// `winner` (`None`: the transaction's own tag). The goal waits until the
    /// answering early dialog's reliable provisional is PRACKed, and every
    /// fork's for an unrung winner (RFC 3262 §3).
    Answer { winner: Option<ForkWinner> },
    /// Reject a [`Disposition::Held`](super::Disposition::Held) callee's INVITE
    /// with the non-2xx final `status`. RFC 3262 §3 allows a non-2xx final
    /// before the PRACK; the goal is stricter and waits until every reliable
    /// provisional the INVITE drew is PRACKed, so no PRACK crosses the final.
    /// The final then awaits its hop-ACK (RFC 3261 §17.2.1).
    Reject { status: u16 },
    /// Send ONE in-dialog OPTIONS keepalive ping on the confirmed dialog and
    /// read its 200 inline — the `long_call` single ping. The first ping stamps
    /// the `keepalive_ack` feed.
    Options,
    /// Loop an in-dialog OPTIONS keepalive ping every `cadence` until `hold`
    /// elapses (the `options_hold` keepalive loop) — each 200 read inline; the
    /// first stamps `keepalive_ack`.
    EveryOptions { cadence: Duration, hold: Duration },
    /// CANCEL the still-pending initial INVITE (RFC 3261 §9.1) — the abandon
    /// path. Keeps the pending INVITE so its `487` still routes to it. §9.1
    /// pins the Request-URI, Via branch, CSeq and identities to the INVITE's, so
    /// `stated` carries the only thing the canceller adds of its own: RFC 3326
    /// §2 scopes `Reason` to CANCEL and BYE, and this is the CANCEL half.
    Cancel { stated: Vec<(String, String)> },
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
        /// The CSeq method of the transaction this expectation is about. `Some`
        /// skips responses belonging to any other transaction on the leg — an
        /// automatic PRACK's 2xx must not satisfy the call's own answer. `None`
        /// takes the next response of the status whatever answered it.
        cseq_method: Option<String>,
        body: BodyExpect,
        early: Option<EarlyId>,
        ack_body: Option<Vec<u8>>,
        matcher: Option<MessageTemplate>,
    },
    /// Strict reception: consume a parked request of this kind into the actor's
    /// bound server transaction (the `RespondTemplate`/`Respond` that follows
    /// answers it). [`RequestKind::Cancel`] consumes WITHOUT rebinding — the
    /// binding stays the INVITE the CANCEL targets. `matcher` runs at consume
    /// time on the parked transaction's request. WHICH parked request is
    /// `super::select::select_parked`: `rank`, then `body`, then arrival.
    ExpectRequest {
        kind: RequestKind,
        body: BodyExpect,
        matcher: Option<MessageTemplate>,
        /// Which request of its kind on this leg the step is about — a 0-based
        /// ordinal in the leg's CSeq order, supplied by the CALLER (upstream
        /// owns the correlation mechanics, never the policy that derives the
        /// number). `None`, and a rank no parked request carries, fall back to
        /// arrival order.
        rank: Option<usize>,
    },
    /// Observed, never asserted: wait for the next final on this leg, record
    /// `(key, expected, observed)` into the replay record, and key the
    /// RFC-compliant follow-up on the OBSERVED status (the reactor's normal
    /// handling — ACK a 2xx, hop-ACK a non-2xx INVITE final). `cseq_method`
    /// pins the observation to ONE transaction: another transaction's final
    /// (a stack-automatic PRACK/CANCEL 2xx) is skipped, never recorded as
    /// this observation's final. `None` takes the leg's next final whatever
    /// transaction carried it.
    ObserveFinal { key: u32, expected: Option<u16>, cseq_method: Option<String> },
    /// Strict with a class-shaped assertion — the truncated-variant anchor.
    /// Follow-up keyed on the observed final, and `cseq_method` pins the
    /// transaction, like [`ObserveFinal`](Self::ObserveFinal).
    ExpectFinal { assert: FinalAssert, cseq_method: Option<String> },
}

impl GoalStep {
    /// The step's bounded kind name — the `<kind>` of a failed-step key
    /// ([`FailedAt`](super::FailedAt)).
    pub fn kind(&self) -> &'static str {
        match self {
            GoalStep::Invite { .. } => "invite",
            GoalStep::Refer { .. } => "refer",
            GoalStep::Reinvite => "reinvite",
            GoalStep::Update => "update",
            GoalStep::UpdateEarly => "update_early",
            GoalStep::EarlyRequest { .. } => "early_request",
            GoalStep::Ring { .. } => "ring",
            GoalStep::Answer { .. } => "answer",
            GoalStep::Reject { .. } => "reject",
            GoalStep::Options => "options",
            GoalStep::EveryOptions { .. } => "every_options",
            GoalStep::Cancel { .. } => "cancel",
            GoalStep::InDialog { .. } => "in_dialog",
            GoalStep::Bye => "bye",
            GoalStep::ByeIfConfirmed => "bye_if_confirmed",
            GoalStep::ByeWith { .. } => "bye_with",
            GoalStep::InviteTemplate { .. } => "invite_template",
            GoalStep::RequestTemplate { .. } => "request_template",
            GoalStep::RespondTemplate { .. } => "respond_template",
            GoalStep::Respond { .. } => "respond",
            GoalStep::ExpectResponse { .. } => "expect_response",
            GoalStep::ExpectRequest { .. } => "expect_request",
            GoalStep::ObserveFinal { .. } => "observe_final",
            GoalStep::ExpectFinal { .. } => "expect_final",
        }
    }

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
    /// The goal is not owed once this holds before its guard does: the call
    /// took a branch where the step has no place (a transfer refused, so the
    /// target's leg never opens).
    pub unless: Option<Barrier>,
}

impl Goal {
    pub fn new(guard: Barrier, step: GoalStep) -> Self {
        Self { guard, delay: Duration::ZERO, step, deadline: None, unless: None }
    }

    /// Drop this goal when `branch` holds before its guard does.
    pub fn unless(mut self, branch: Barrier) -> Self {
        self.unless = Some(branch);
        self
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
    /// The pending goal's guard-wait deadline, anchored at its FIRST poll and
    /// kept across re-polls — without the anchor every reactor wake (e.g. the
    /// 2 s recv timeout) would refresh the bound and a stuck guard could dodge
    /// its timeout indefinitely.
    deadline_at: Option<Instant>,
}

impl GoalCursor {
    pub fn new(goals: Vec<Goal>) -> Self {
        Self { goals, cursor: 0, ready_at: None, deadline_at: None }
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

    /// The cursor's index — the goal-list position of the next un-fired goal
    /// (the `step` an `AcceptedDelta` observation names).
    pub(super) fn position(&self) -> usize {
        self.cursor
    }

    /// The pending goal as `(kind, index)` — the goal a failure of this actor
    /// is attributed to. `None` once every goal has fired.
    pub(super) fn pending(&self) -> Option<(&'static str, u16)> {
        let step = self.next_step()?;
        Some((step.kind(), u16::try_from(self.cursor).unwrap_or(u16::MAX)))
    }

    /// Drop every un-fired goal: the actor's call ended on an accepted final
    /// before they could fire, so none of them is owed.
    pub(super) fn abandon(&mut self) {
        self.cursor = self.goals.len();
        self.ready_at = None;
        self.deadline_at = None;
    }

    /// The steps of every un-fired goal, in order — the park-or-react match set.
    pub(super) fn remaining_steps(&self) -> impl Iterator<Item = &GoalStep> {
        self.goals[self.cursor.min(self.goals.len())..].iter().map(|g| &g.step)
    }

    /// Resolve once the next pending goal's guard holds (immediately for
    /// [`Barrier::None`]) and its post-guard dwell has elapsed, returning the
    /// step to drive, or `None` when its [`unless`](Goal::unless) branch held
    /// first (the goal is not owed: the caller advances past it) or `moot`
    /// held (every remaining goal is dropped) — the guard
    /// wait is bounded by `timeout` so a genuinely stuck guard fails the actor
    /// rather than hanging. Parks forever if the cursor is exhausted (the
    /// reactor gates this arm on [`has_pending`](Self::has_pending), so the
    /// park is never actually polled).
    pub async fn next_ready(
        &mut self,
        obs: &ObservedState,
        timeout: Duration,
        moot: impl Fn(&super::state::StateInner) -> bool,
    ) -> Result<Option<GoalStep>, StepError> {
        let Some(goal) = self.goals.get(self.cursor) else {
            return std::future::pending().await;
        };
        // Lift the guard/delay out so the dwell anchor below can borrow `self`
        // mutably (the guard is Arc-backed, the clone is cheap).
        let (guard, delay, bound) = (goal.guard.clone(), goal.delay, goal.deadline);
        let unless = goal.unless.clone();
        let deadline =
            *self.deadline_at.get_or_insert_with(|| Instant::now() + bound.unwrap_or(timeout));
        let skipped = |s: &super::state::StateInner| unless.as_ref().is_some_and(|b| b.holds(s));
        await_pred(obs, guard.name(), |s| guard.holds(s) || skipped(s) || moot(s), deadline)
            .await?;
        if obs.with_snapshot(&moot) {
            self.abandon();
            return Ok(None);
        }
        if !obs.with_snapshot(|s| guard.holds(s)) {
            return Ok(None);
        }
        if !delay.is_zero() {
            let at = *self.ready_at.get_or_insert(Instant::now() + delay);
            tokio::time::sleep_until(at).await;
        }
        Ok(Some(self.goals[self.cursor].step.clone()))
    }

    /// Advance past the goal that just fired (resets the dwell and deadline
    /// anchors for the next goal).
    pub fn advance(&mut self) {
        self.cursor += 1;
        self.ready_at = None;
        self.deadline_at = None;
    }
}
