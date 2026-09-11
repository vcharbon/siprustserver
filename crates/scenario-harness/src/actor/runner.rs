//! The live per-endpoint state ([`ActorState`]) and the reactor loop
//! ([`run_actor`]) — the `select!` that races inbound reactions, the goal
//! cursor, the timed answer, the reject-hop-ACK watch, and the glare retries
//! on ONE task. What each arm DOES lives in its own module: reactions in
//! [`super::react`]/[`super::response`], goal steps in [`super::drive`].
//!
//! The actor owns **no retransmit timers** — it answers idempotently and keeps
//! its inbox open; the transport (`loadgen::mux::CallTxns`) or the SUT owns
//! retransmission. Its only obligation is to stay reactive long enough for
//! those retransmitters to heal a loss (the ack-gated settle barrier,
//! [`super::settle`]).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use sip_message::{CseqDeviation, DelayedAutomatic};

use super::answer::fire_timed_answer;
use super::delta::AcceptedDeltaPolicy;
use super::drive::drive_goal;
use super::endpoint::{ActorSpec, Automatics, CtxFeed, Disposition, MediaState};
use super::goals::{GoalCursor, GoalStep, RequestKind};
use super::ledger::ObligationKey;
use super::observe::ReceptionObserver;
use super::originate::{
    originate_reinvite, originate_update, wait_reinvite_retry, wait_update_retry,
};
use super::react::default_react;
use super::script::{goal_arm_enabled, requeue_parked};
use super::shared_endpoint::{EndpointHandle, Inbox};
use super::state::{Observation, ObservedState};
use crate::realcall::{CallCtx, CallScope, ChallengeResponder};
use crate::{Agent, ClientInvite, Dialog, ServerTxn, StepError};

/// One inbound request parked on a [`Disposition::Scripted`] actor, awaiting
/// the reception goal that consumes it (or a requeue-on-advance auto-react).
pub(super) struct ParkedRequest {
    pub(super) txn: ServerTxn,
    /// Whether this is the dialog-creating INVITE (no To-tag).
    pub(super) initial: bool,
}

/// The forking-UAS answer plan carried from the 18x emission to the moment the
/// INVITE is answered (the timed-answer arm, or the winner fork's PRACK): the
/// `200` goes out under `winner_tag` (adopted as the transaction's dialog tag),
/// then optionally a LATE `200` under `loser_late_200`.
#[derive(Debug, Clone, Copy)]
pub(super) struct ForkAnswer {
    pub(super) winner_tag: &'static str,
    pub(super) loser_late_200: Option<&'static str>,
}

/// A ring/answer scheduled for `at` — held as its own interruptible arm so a
/// CANCEL that lands before it fires answers `487` on the retained INVITE txn
/// instead of racing an inline sleep.
pub(super) struct TimedAnswer {
    pub(super) at: Instant,
    /// The pending UAS INVITE transaction — answered `200` when the timer fires
    /// OR `487` if a CANCEL arrives first.
    pub(super) uas: ServerTxn,
    /// A forking callee's answer plan (`None` for the plain ring→answer): the
    /// `200` goes out under the winning fork's tag (+ optional loser late 200).
    pub(super) fork: Option<ForkAnswer>,
}

/// One ACK-to-2xx a declared delayed automatic is holding: its own `select!`
/// arm sends it (idempotent, re-derivable — RFC 3261 §13.2.2.4) when `at`
/// elapses and completes the re-INVITE bookkeeping the hold deferred. A 2xx
/// retransmitted while the entry is pending is absorbed, never ACKed early.
pub(super) struct HeldAck {
    pub(super) at: Instant,
    /// The held transaction — the originated in-dialog INVITE's CSeq.
    pub(super) cseq: u32,
    /// The ACK body resolved when the 2xx first arrived (byte-identical on the
    /// eventual send, §13.2.2.4).
    pub(super) sdp: Option<String>,
}

/// A non-2xx final we sent to an initial INVITE, awaiting its §17.1.1.3
/// hop-ACK. The receive core claims that ACK below `recv_any` (it never
/// surfaces to the reactor), so the actor watches the transaction
/// layer's fulfilment as its own `select!` arm and closes the `reject-final`
/// ledger obligation there. This is what makes the UA outlive the call: a
/// REJECTED leg abandoned by a reroute keeps its actor reactive (and the
/// settle barrier holds the verdict + recording window, Timer-H-bounded) until
/// a lost hop-ACK is recovered by the Timer-G final retransmit + the SUT's
/// §17.1.1.2 re-ACK.
pub(super) struct PendingRejectAck {
    pub(super) key: ObligationKey,
    pub(super) call_id: String,
    pub(super) branch: String,
}

/// The confirmed dialog(s) + pending INVITE transaction an endpoint owns.
/// Deliberately minimal: one caller INVITE, one confirmed dialog — enough for
/// every current body incl. the realign flows, which ride the confirmed dialog
/// as in-dialog UAS transactions. (The reliable-183 / `Reject` pending-UAS
/// holds live as [`ActorState`] fields, not here.)
#[derive(Default)]
pub(super) struct DialogTable {
    /// The caller's outgoing INVITE, awaiting its confirmation (learned from the
    /// responses the reactor feeds it via [`ClientInvite::absorb_response`]).
    pub(super) pending_invite: Option<ClientInvite>,
    /// Our confirmed dialog (caller after ACK, or UAS after answering).
    pub(super) confirmed: Option<Dialog>,
    /// The caller's establishing INVITE, RETAINED after confirmation (C1/E3):
    /// a LOSING fork's late 2xx (§13.2.2.4) arrives after the winner's and must
    /// be ACK+BYE'd on ITS OWN fork dialog — derived from this transaction
    /// ([`ClientInvite::fork_dialog`]), never from the confirmed (winner) one.
    pub(super) won_invite: Option<ClientInvite>,
}

/// The live per-endpoint state driven by [`run_actor`].
pub struct ActorState<'c> {
    pub(super) role: &'static str,
    pub(super) agent: Agent,
    pub(super) disposition: Disposition,
    pub(super) media: MediaState,
    pub(super) dialogs: DialogTable,
    pub(super) pending_answer: Option<TimedAnswer>,
    pub(super) goals: GoalCursor,
    pub(super) obs: ObservedState,
    pub(super) scope: Arc<CallScope>,
    pub(super) ctx: &'c CallCtx,
    pub(super) step_timeout: Duration,
    pub(super) invite_targets: HashMap<&'static str, Agent>,
    pub(super) via: Option<SocketAddr>,
    pub(super) feed: CtxFeed,
    /// Whether this caller has already seen (and anchored) a >100 provisional.
    pub(super) saw_provisional: bool,
    /// CSeq numbers of in-dialog re-INVITEs this leg has ANSWERED (200 sent,
    /// ACK outstanding) — the matching ACK advances the realign sub-flow.
    pub(super) answered_reinvites: HashSet<u32>,
    /// A reliable-`183` answer (RFC 3262) awaiting the caller's PRACK: the held
    /// UAS INVITE transaction, answered `200` only once the PRACK arrives
    /// (MUST-014). `Some` for a [`Disposition::ReliableAnswer`] leg between its
    /// 183 and the PRACK.
    pub(super) pending_prack_answer: Option<ServerTxn>,
    /// A [`Disposition::RingThenSilent`] leg's held INVITE server transaction
    /// (180 sent, no final EVER originated by this leg): released only by an
    /// inbound CANCEL — the SUT's no-answer timer firing — which 487s it.
    pub(super) held_silent: Option<ServerTxn>,
    /// `(fork To-tag, RSeq)` pairs of reliable provisionals this caller has
    /// already PRACKed — so a retransmitted 183 is not double-PRACKed, while
    /// a FORKED reliable 183 (distinct tag, same RSeq space per §12.1.2 — each
    /// fork typically starts at RSeq 1) still gets its OWN PRACK (C1/E3).
    pub(super) pracked_rseqs: HashSet<(String, u32)>,
    /// A held forking-UAS answer plan (C1/E3): set alongside
    /// `pending_prack_answer` by a RELIABLE [`Disposition::ForkingRing`], so the
    /// PRACK arm answers the INVITE only on the WINNER fork's PRACK (a losing
    /// fork's PRACK is 200'd but does not release the 200-to-INVITE).
    pub(super) fork_answer: Option<ForkAnswer>,
    /// The LOSING fork tags this forking callee emitted a late `200` under — an
    /// inbound BYE addressed to one of these tears down only that early fork,
    /// NOT this leg (the winning dialog lives on; the BYE is 200'd and its CSeq
    /// folded into the dialog stream, but no `LegTerminated` is recorded).
    pub(super) fork_loser_tags: HashSet<String>,
    /// CSeq numbers of in-dialog re-INVITEs THIS caller has ORIGINATED whose
    /// `reneg` sub-flow has not yet been advanced (the `reinvite` body's
    /// delayed-offer re-INVITE). A set keyed by CSeq — NOT a one-shot bool — so
    /// the 2xx ACK is re-derivable and a lost datagram interleaving can never
    /// strand it (mirrors the mux's `(Call-ID, CSeq)` re-ACK). Empty for every
    /// non-`reinvite` leg.
    pub(super) sent_reinvites: HashSet<u32>,
    /// The client transaction handle of each outstanding originated re-INVITE,
    /// keyed by CSeq — retained so a NON-2xx final (a `491 Request Pending`
    /// glare reject, C4/S5) can be hop-ACKed (§17.1.1.3): `recv_any` surfaces
    /// the 491 as a bare response without auto-ACKing it. Cleared in lockstep
    /// with `sent_reinvites` on the 2xx OR the 491.
    pub(super) sent_reinvite_txns: HashMap<u32, crate::InDialogTxn>,
    /// A pending §14.1 re-INVITE glare RETRY (C4/S5): set when our re-INVITE
    /// drew a 491, fires after the owner/non-owner dwell to re-originate it.
    pub(super) reinvite_retry: Option<Instant>,
    /// CSeq numbers of in-dialog UPDATEs we originated awaiting their 200 — an
    /// OUTSTANDING OFFER (RFC 3311 §5.1). Both this and `sent_reinvites`
    /// represent an outstanding offer, so an incoming offer-bearing UPDATE or
    /// re-INVITE while EITHER is non-empty is 491'd (C4/S6 collision).
    pub(super) sent_updates: HashSet<u32>,
    /// A pending UPDATE-collision RETRY (C4/S6): set when our UPDATE drew a 491,
    /// fires after the §14.1-style back-off to re-originate it (UPDATE has no
    /// ACK, so the 491 alone completes its transaction).
    pub(super) update_retry: Option<Instant>,
    /// C5: a [`Disposition::ReliableAnswerEarlyUpdate`] callee HOLDS the INVITE
    /// 200 across the PRACK, answering it only after an early UPDATE is 200'd
    /// (RFC 3311 §5.1). `true` only for that disposition.
    pub(super) hold_for_early_update: bool,
    /// C5 ordering: whether this early-UPDATE callee has PRACKed its reliable
    /// 183 (RFC 3262 MUST-014 — the 2xx must not precede the PRACK) and whether
    /// the early UPDATE has been 200'd. The held INVITE 200 is released only
    /// once BOTH hold, so a UPDATE that races ahead of the PRACK does not answer
    /// the INVITE early.
    pub(super) early_pracked: bool,
    pub(super) early_updated: bool,
    /// Whether this caller has already stamped the first-OPTIONS-ping feed —
    /// so the looped `options_hold` pings stamp `keepalive_ack` exactly once.
    pub(super) saw_options_200: bool,
    /// The provisional this caller's establishing INVITE awaits — `180` by
    /// default, `183` once it has advertised `Supported: 100rel` (the reliable
    /// flows). The `expected` field of the incidental `WrongStatus` a shed/reject
    /// on the establishing INVITE surfaces (linear `establish`/`establish_100rel`
    /// parity).
    pub(super) expected_provisional: u16,
    /// A sent non-2xx initial-INVITE final whose hop-ACK is outstanding — its
    /// own `select!` arm closes the `reject-final` obligation on fulfilment.
    /// At most one per leg (a leg rejects its initial INVITE once, or 487s its
    /// one CANCELled ring).
    pub(super) pending_reject_ack: Option<PendingRejectAck>,
    /// The **deferred-auth adapter** (RFC 3261 §22.2) wired onto this caller's
    /// establishing INVITE. `Some` → a `401`/`407` to that INVITE is ACKed, the
    /// responder is asked for a credential, and the INVITE is resent ONCE
    /// (bumped CSeq, fresh branch — see [`ClientInvite::ack_and_resend_with_auth`]);
    /// `None` (the default) → a challenge classifies as `status_401/407`
    /// unchanged. Reaches the caller from [`CallEnv::challenge_responder`]
    /// (`crates/loadgen/src/driver.rs` `run_one`).
    pub(super) challenge_responder: Option<Arc<dyn ChallengeResponder>>,
    /// Authenticated INVITE resends still permitted (RFC 3261 §22.2) — `1` when a
    /// [`challenge_responder`](Self::challenge_responder) is wired, else `0`.
    /// Capped so a challenge to the *resent* INVITE surfaces as a plain
    /// `status_401/407` deviation, never an unbounded loop.
    pub(super) auth_retries_left: u8,
    /// A [`Disposition::Scripted`] actor's parked inbound requests, in arrival
    /// order — reception goals consume them; requeue-on-advance auto-reacts
    /// what no remaining goal can consume.
    pub(super) parked: Vec<ParkedRequest>,
    /// The automatic that consumed the script-claimed initial INVITE — parked
    /// OR bound (CANCEL → 487): a later scripted step bound to it fails fast
    /// naming this, never by timeout.
    pub(super) parked_initial_consumed: Option<&'static str>,
    /// The server transaction the nearest preceding `ExpectRequest` consumed —
    /// what a `RespondTemplate`/`Respond` goal answers.
    pub(super) bound: Option<ServerTxn>,
    /// This actor's cursor into its leg's ordered response-fact log — the
    /// consumption point of the reception goals.
    pub(super) resp_seen: usize,
    /// How many requests of each kind the script has consumed on this leg — the
    /// anchor a parked request's CSeq rank is counted from
    /// ([`super::select::select_parked`]).
    pub(super) consumed_requests: HashMap<RequestKind, usize>,
    /// The ACK body resolved for each in-dialog INVITE 2xx we ACKed, keyed by
    /// CSeq (`None` = the bodyless ACK a complete offer/answer round takes) —
    /// the ACK to a RETRANSMITTED 2xx must be byte-identical (RFC 3261
    /// §13.2.2.4), so the decision is resolved once and re-emitted verbatim,
    /// never re-derived from the (advanced) goal cursor or a dropped transaction.
    pub(super) reinvite_ack_bodies: HashMap<u32, Option<String>>,
    /// The plan's lane-chosen stack automatics.
    pub(super) automatics: Automatics,
    /// This actor's ONE CSeq deviation counter (ADR-0024 §6): a shared handle
    /// attached at EVERY dialog-formation point, so all of the leg's dialogs and
    /// their scope-refresh clones number from one step sequence. `None` = stack
    /// numbering.
    pub(super) cseq_dev: Option<Arc<Mutex<CseqDeviation>>>,
    /// Declared delayed automatics (ADR-0024 §6) on this actor's ACK-to-2xx —
    /// per-transaction scoped or unscoped ([`DelayedAutomatic::invite_ordinal`]).
    /// Empty = every ACK fires immediately.
    pub(super) delayed: Vec<DelayedAutomatic>,
    /// Count of INVITE transactions this actor has ORIGINATED (establishing
    /// INVITE, then each in-dialog INVITE, retries included) — the ordinal
    /// space a scoped [`DelayedAutomatic`] names.
    pub(super) originated_invites: usize,
    /// Each outstanding originated re-INVITE's ordinal in that space, by CSeq —
    /// how the 2xx ACK path resolves whether ITS automatic is held.
    pub(super) reinvite_ordinals: HashMap<u32, usize>,
    /// ACKs to re-INVITE 2xx currently HELD by a delayed automatic, fired by
    /// their own reactor arm — so goal progress (the deviation's point: traffic
    /// DURING the hold) never blocks on the hold.
    pub(super) held_acks: Vec<HeldAck>,
    /// Whether this actor ORIGINATES the dialog — its first goal is an
    /// `Invite`/`InviteTemplate` (fallback: `Disposition::Caller`). Keys the
    /// §14.1 glare owner dwell and the caller attribution.
    pub(super) originates: bool,
    /// The plan's accepted-delta policy (ADR-0024 §6): consulted when a due
    /// reception expectation is confronted with a non-matching but
    /// classifiable inbound, BEFORE the mismatch path. `None` = the hook is
    /// absent and behavior is unchanged.
    pub(super) delta_policy: Option<AcceptedDeltaPolicy>,
    /// The plan's reception observer: invoked with the typed message each
    /// time one of this actor's reception goals consumes one. Purely
    /// observational — it cannot change the run's outcome. `None` = the hook
    /// is absent and behavior (including message retention) is unchanged.
    pub(super) reception_observer: Option<ReceptionObserver>,
    /// The distinct To-tags this UAS has emitted >100 provisionals under on
    /// its pending initial INVITE (`""` = the transaction's default sticky
    /// tag) — the [`DialogSnapshot::early_dialog_count`] source. Read only
    /// while an initial-INVITE target is still pending.
    pub(super) early_provisionals: HashSet<String>,
    /// Where this actor's inbound comes from: its own UA (the unshared default)
    /// or the shared endpoint's pump. Taken by [`run_actor`] at entry.
    inbox: Option<Inbox>,
    /// This actor's registration on its SHARED endpoint — a dialog it
    /// originates is bound to it here so the dialog's traffic comes back.
    /// `None` on an unshared endpoint.
    pub(super) endpoint: Option<EndpointHandle>,
}

impl<'c> ActorState<'c> {
    /// Wire a declarative [`ActorSpec`] to the shared observed state, teardown
    /// scope, and timing context. `step_timeout` bounds each goal-guard wait.
    #[allow(clippy::too_many_arguments)] // the plan-knob fan-out mirrors `CallPlan`
    pub fn from_spec(
        spec: ActorSpec,
        obs: ObservedState,
        scope: Arc<CallScope>,
        ctx: &'c CallCtx,
        step_timeout: Duration,
        challenge_responder: Option<Arc<dyn ChallengeResponder>>,
        automatics: Automatics,
        delta_policy: Option<AcceptedDeltaPolicy>,
        reception_observer: Option<ReceptionObserver>,
    ) -> Self {
        let originates = spec.goals.first().is_some_and(|g| {
            matches!(g.step, GoalStep::Invite { .. } | GoalStep::InviteTemplate { .. })
        }) || matches!(spec.disposition, Disposition::Caller);
        Self {
            role: spec.role,
            agent: spec.agent,
            disposition: spec.disposition,
            media: spec.media,
            dialogs: DialogTable::default(),
            pending_answer: None,
            goals: GoalCursor::new(spec.goals),
            obs,
            scope,
            ctx,
            step_timeout,
            invite_targets: spec.invite_targets.into_iter().collect(),
            via: spec.via,
            feed: spec.feed,
            saw_provisional: false,
            answered_reinvites: HashSet::new(),
            pending_prack_answer: None,
            held_silent: None,
            pracked_rseqs: HashSet::new(),
            fork_answer: None,
            fork_loser_tags: HashSet::new(),
            sent_reinvites: HashSet::new(),
            sent_reinvite_txns: HashMap::new(),
            reinvite_retry: None,
            sent_updates: HashSet::new(),
            update_retry: None,
            hold_for_early_update: matches!(
                spec.disposition,
                Disposition::ReliableAnswerEarlyUpdate
            ),
            early_pracked: false,
            early_updated: false,
            saw_options_200: false,
            expected_provisional: 180,
            pending_reject_ack: None,
            auth_retries_left: if challenge_responder.is_some() { 1 } else { 0 },
            challenge_responder,
            parked: Vec::new(),
            parked_initial_consumed: None,
            bound: None,
            resp_seen: 0,
            consumed_requests: HashMap::new(),
            reinvite_ack_bodies: HashMap::new(),
            automatics,
            cseq_dev: spec
                .cseq
                .filter(|p| !p.is_identity())
                .map(|p| Arc::new(Mutex::new(CseqDeviation::new(p)))),
            delayed: spec.delayed,
            originated_invites: 0,
            reinvite_ordinals: HashMap::new(),
            held_acks: Vec::new(),
            originates,
            delta_policy,
            reception_observer,
            early_provisionals: HashSet::new(),
            inbox: None,
            endpoint: None,
        }
    }

    /// Seat this actor at a SHARED endpoint: inbound arrives through the
    /// endpoint's one pump, and every dialog this actor originates is bound to
    /// it so the dialog's responses and in-dialog requests come back here.
    pub(super) fn on_shared_endpoint(mut self, inbox: Inbox, handle: EndpointHandle) -> Self {
        self.inbox = Some(inbox);
        self.endpoint = Some(handle);
        self
    }

    /// The declared hold for the `ordinal`-th INVITE transaction this actor
    /// originates: a deviation scoped to that ordinal wins over an unscoped one
    /// (which holds every ACK-to-2xx). `None` = the ACK fires promptly.
    pub(super) fn ack_hold_for(&self, ordinal: usize) -> Option<Duration> {
        let acks = || self.delayed.iter().filter(|d| d.which == sip_message::Automatic::AckTo2xx);
        acks()
            .find(|d| d.invite_ordinal == Some(ordinal))
            .or_else(|| acks().find(|d| d.invite_ordinal.is_none()))
            .map(|d| Duration::from_millis(d.delay_ms))
    }

    /// The SDP body to answer an INVITE/UPDATE with — this endpoint's answer (or
    /// offer) media, falling back to the crate default so an answer-to-INVITE is
    /// NEVER bodyless (RFC 3264 §5), even for a signalling-only endpoint
    /// ([`MediaState::none`]). A delayed-offer bodyless re-INVITE thus still gets
    /// 200 + our SDP.
    pub(super) fn answer_body(&self) -> &'static str {
        self.media.answer_sdp().unwrap_or(crate::ANSWER_SDP)
    }
}

/// Drive ONE endpoint: interleave reacting with goal progress via `select!`, so
/// a goal parked on a barrier NEVER blocks the reactor (the structural fix for
/// the cascade). Resolves `Ok(())` when the call is fully torn down and this
/// endpoint's goals are exhausted, or `Err` on a fatal step.
pub async fn run_actor(mut st: ActorState<'_>) -> Result<(), StepError> {
    // Clone the Arc-backed handles the reactor arm needs, so the goal / timed
    // arms can borrow disjoint fields of `st` in the same `select!`.
    let agent = st.agent.clone();
    let obs = st.obs.clone();
    let step_timeout = st.step_timeout;
    // Inbound source: this actor's own UA, or — when several actors share the
    // endpoint — the seat its pump delivers to. Same vocabulary either way.
    let mut inbox = st.inbox.take().unwrap_or_else(|| Inbox::Own(agent.clone()));
    loop {
        tokio::select! {
            inbound = inbox.recv() => {
                match inbound {
                    Ok(m) => default_react(&mut st, m).await?,
                    // A reactor recv deadline is NOT fatal — loop again (a
                    // long_call / options_hold / the 32 s settle silence must
                    // not kill the actor). Only a closed queue is fatal.
                    Err(StepError::Timeout { .. }) => {}
                    Err(StepError::QueueClosed { .. }) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            // Reception goals are additionally gated on their consumable being
            // observable (a new response fact / a matching parked request / a
            // bound transaction) — the wait rides THIS arm, never `drive_goal`
            // (a wait inside the body would starve the reactor: the documented
            // inline-pull hazard). Every consumable appears via this actor's
            // own loop body, so the gate is re-evaluated on each iteration.
            ready = st.goals.next_ready(&obs, step_timeout), if st.goals.has_pending() && goal_arm_enabled(&st) => {
                let step = ready?;
                drive_goal(&mut st, step).await?;
                st.goals.advance();
                // Requeue on advance: auto-react any parked request no
                // remaining goal can consume, so it never starves.
                requeue_parked(&mut st).await?;
            }
            _ = wait_timed_answer(&st.pending_answer), if st.pending_answer.is_some() => {
                fire_timed_answer(&mut st).await?;
            }
            // Our non-2xx final's hop-ACK was sighted (the receive core claims
            // it below `recv_any`, so it never surfaces as an inbound) — close
            // the reject-final obligation. This arm is also the wake that lets
            // the exit check below run once the ledger closes.
            _ = wait_reject_ack(&agent, &st.pending_reject_ack), if st.pending_reject_ack.is_some() => {
                if let Some(p) = st.pending_reject_ack.take() {
                    st.obs.record(Observation::ResponseObserved { key: p.key }, Instant::now());
                }
            }
            // The §14.1 re-INVITE glare retry deadline (C4/S5): re-originate the
            // re-INVITE now the owner/non-owner back-off has elapsed — the peer's
            // own re-INVITE was 491'd and is no longer pending, so this retry is
            // 200'd and the round completes.
            _ = wait_reinvite_retry(&st.reinvite_retry), if st.reinvite_retry.is_some() => {
                st.reinvite_retry = None;
                originate_reinvite(&mut st).await?;
            }
            // The S6 UPDATE-collision retry deadline (C4/S6): re-originate the
            // UPDATE now the peer's colliding offer has cleared.
            _ = wait_update_retry(&st.update_retry), if st.update_retry.is_some() => {
                st.update_retry = None;
                originate_update(&mut st).await?;
            }
            // A held ACK-to-2xx (a declared delayed automatic) coming due: send
            // it and complete the bookkeeping the hold deferred — the eventual
            // send is THIS arm's obligation, so no call finishes un-ACKed.
            _ = super::response::wait_held_ack(&st.held_acks), if !st.held_acks.is_empty() => {
                super::response::fire_due_held_ack(&mut st).await;
            }
        }
        // `ledger_closed` keeps a leg with an outstanding acknowledgement (its
        // own reject-final, or any leg's open obligation) REACTIVE through the
        // settle window — the UA outlives the call, so a re-emitted final /
        // recovered ACK is consumed, closed, and recorded rather than orphaned.
        // Bounded: the controller's settle ceiling (64·T1) wins the outer
        // `select!` and drops still-parked actors either way.
        if obs.all_terminated() && st.goals.is_exhausted() && obs.ledger_closed() {
            return Ok(());
        }
    }
}

/// Park until the scheduled timed answer is due (or forever if none) — the
/// interruptible ring→answer arm.
async fn wait_timed_answer(pending: &Option<TimedAnswer>) {
    match pending {
        Some(ta) => tokio::time::sleep_until(ta.at).await,
        None => std::future::pending().await,
    }
}

/// Park until the pending reject-final's hop-ACK is sighted (or forever if
/// none) — the fulfilment arm for the obligation [`arm_reject_final`] opened.
async fn wait_reject_ack(agent: &Agent, pending: &Option<PendingRejectAck>) {
    match pending {
        Some(p) => agent.hop_ack_fulfilled(&p.call_id, &p.branch).await,
        None => std::future::pending().await,
    }
}
