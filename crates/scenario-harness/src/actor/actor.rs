//! The **endpoint actor** — one `tokio` future per SIP endpoint that runs a
//! reactive answering loop concurrently with its scripted goal cursor, joined
//! (NOT spawned) on the one per-call task (see [`super`]).
//!
//! Inside the actor, three arms race in a `select!`:
//! - **Reactor** ([`default_react`]) — answers whatever arrives whenever it
//!   arrives (re-INVITE→200+SDP, NOTIFY→200, OPTIONS→200, BYE→200+terminate,
//!   CANCEL→487, ACK→absorb) and folds each observation into the shared
//!   [`ObservedState`]. Because it is reactive, late / reordered / retransmitted
//!   datagrams are always consumed — the cascade's "unconsumed"/"absorbed
//!   retransmit" anomaly disappears by construction.
//! - **Goal cursor** ([`super::goals`]) — the next scripted intent, once its
//!   barrier guard holds. A goal parked on a barrier never blocks the reactor.
//! - **Timed answer** — a ring→answer scheduled for later, as its OWN arm (B6),
//!   so a CANCEL mid-ring is still processed (never an inline `sleep` in
//!   `default_react`).
//!
//! The actor owns **no retransmit timers** — it answers idempotently and keeps
//! its inbox open; the transport (`loadgen::mux::CallTxns`) or the SUT owns
//! retransmission. Its only obligation is to stay reactive long enough for those
//! retransmitters to heal a loss (the ack-gated settle barrier, [`super::settle`]).
//!
//! # Downstream-contract feeding is DECLARATIVE ([`CtxFeed`])
//!
//! Phases / checkpoints / the 18x ringing gate key the load report's case
//! buckets and the chaos classifier's phase-transition proximity (see
//! `docs/todos/actor-harness-p1-contract-table.md`), and each linear body
//! stamps a DIFFERENT trail (the refer body stamps only `referred`/
//! `transferred` and never feeds `mark_ringing`; basic stamps `connected`/
//! `bye_200` and does). So the reactor stamps NOTHING on its own — each
//! [`ActorSpec`] declares exactly which reactive event feeds which label, and
//! an undeclared event feeds nothing. Message ANCHORS are the exception: they
//! are attached generically at reaction time with the message in hand (they are
//! inert unless the shape publishes them and the call is sampled).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::goals::{Goal, GoalCursor, GoalStep};
use super::ledger::{ObligationKey, ObligationKind};
use super::state::{Observation, ObservedState, SubflowState};
use sip_message::generators::InDialogMethod;
use sip_message::SipResponse;

use crate::agent::InviteResponseFate;
use crate::realcall::{CallCtx, CallScope};
use crate::{Agent, ClientInvite, Dialog, Inbound, ServerTxn, StepError};

/// The realign sub-flow name every leg's re-INVITE confirm progress is tracked
/// under (the refer `merged` barrier is a conjunction over these).
pub const SUBFLOW_REALIGN: &str = "realign";
/// The sub-flow name a REFER's acceptance (202) advances on the sending leg.
pub const SUBFLOW_REFER: &str = "refer";
/// The sub-flow name a CALLER-initiated in-dialog renegotiation (a re-INVITE's
/// answered-and-ACKed 2xx, or an UPDATE's 200) advances on the sending leg —
/// the barrier the `reinvite` / `prack_update` teardown gates on so the BYE
/// never races the renegotiation's completion.
pub const SUBFLOW_RENEG: &str = "reneg";

/// How an endpoint answers the INITIAL (dialog-creating) INVITE it receives —
/// the endpoint state machine's entry policy (B6). Later in-dialog traffic is
/// always handled reactively by [`default_react`], regardless of disposition.
#[derive(Debug, Clone, Copy)]
pub enum Disposition {
    /// Originates the call; never answers an initial INVITE.
    Caller,
    /// Answers immediately with `200` + the answer SDP (no provisional).
    Answer,
    /// Rings (`180`) then answers `200` after `ring` — an interruptible timed
    /// answer (a CANCEL mid-ring yields `487`, not a stuck answer). A ZERO ring
    /// still emits the 180 (the linear bodies' 180-then-immediate-200 shape).
    RingThenAnswer { ring: Duration },
    /// Rejects the initial INVITE with a final `code` (486/603/…).
    Reject(u16),
    /// Answers RELIABLY (RFC 3262): a `183` carrying `Require:100rel` + `RSeq` +
    /// the answer SDP, then HOLDS the INVITE transaction, answering `200` to the
    /// INVITE only after the caller PRACKs (MUST-014 ordering). The
    /// rerouting/prack winning-leg disposition.
    ReliableAnswer,
}

/// The offer/answer SDP an endpoint negotiates with.
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaState {
    offer: Option<&'static str>,
    answer: Option<&'static str>,
}

impl MediaState {
    /// A caller's media (carries the offer on the INVITE).
    pub fn offer(sdp: &'static str) -> Self {
        Self { offer: Some(sdp), answer: None }
    }

    /// A callee's media (carries the answer on the 2xx).
    pub fn answer(sdp: &'static str) -> Self {
        Self { offer: None, answer: Some(sdp) }
    }

    /// Both sides: `offer` rides an originated INVITE, `answer` every answer we
    /// send (a caller that also answers realign re-INVITEs — the refer alice).
    pub fn full(offer: &'static str, answer: &'static str) -> Self {
        Self { offer: Some(offer), answer: Some(answer) }
    }

    /// No media (a signalling-only endpoint).
    pub fn none() -> Self {
        Self::default()
    }

    /// The SDP to answer an inbound offer with — the answer if set, else the
    /// offer (a symmetric endpoint). Used for the 2xx and for reactive re-INVITE
    /// answers; NEVER a bodyless 200 to an offer (RFC 3264 §5).
    fn answer_sdp(&self) -> Option<&'static str> {
        self.answer.or(self.offer)
    }

    /// The SDP to offer on an originated INVITE.
    fn offer_sdp(&self) -> Option<&'static str> {
        self.offer
    }
}

/// One optional `(checkpoint, phase)` stamp pair a reactive event feeds — both
/// default to "stamp nothing" (see the module doc on declarative feeding).
#[derive(Debug, Clone, Copy, Default)]
pub struct Feed {
    pub checkpoint: Option<&'static str>,
    pub phase: Option<&'static str>,
}

impl Feed {
    pub const NONE: Feed = Feed { checkpoint: None, phase: None };

    pub fn new(checkpoint: Option<&'static str>, phase: Option<&'static str>) -> Self {
        Self { checkpoint, phase }
    }

    fn stamp(&self, ctx: &CallCtx) {
        if let Some(cp) = self.checkpoint {
            ctx.checkpoint(cp);
        }
        if let Some(ph) = self.phase {
            ctx.phase(ph);
        }
    }
}

/// Which reactive events feed the per-call [`CallCtx`] — the per-body
/// downstream contract (phases / checkpoints / the 18x gate), declared on the
/// spec instead of hardwired in the reactor. Defaults stamp NOTHING.
#[derive(Debug, Clone, Copy, Default)]
pub struct CtxFeed {
    /// Feed `ctx.mark_ringing` from this caller's 18x/answer observations (the
    /// cross-call >99% gate). ONLY the shared-establishment bodies feed it —
    /// the hand-rolled refer/abandon bodies must NOT (contract table §3).
    pub ringing_gate: bool,
    /// Stamped when this caller's establishing INVITE is answered (2xx
    /// received) — `time_to_200` on every current body.
    pub on_answer_rx: Feed,
    /// Stamped on this caller's FIRST >100 provisional (18x/183) — the abandon
    /// body's `time_to_180`. Distinct from the ringing gate (which is a rate,
    /// not a checkpoint).
    pub on_provisional: Feed,
    /// Stamped when the 2xx to this caller's delayed-offer re-INVITE arrives
    /// (after it is ACKed) — the `reinvite` flow's `time_to_reinvite_200` +
    /// `reinvited`.
    pub on_reinvite_ok: Feed,
    /// Stamped when the 200 to this caller's in-dialog UPDATE arrives — the
    /// `prack_update` flow's `time_to_update_200` + `updated`.
    pub on_update_ok: Feed,
    /// Stamped when the 200 to this caller's FIRST in-dialog OPTIONS keepalive
    /// ping arrives — the keepalive flows' `time_to_options_200` +
    /// `keepalive_ack` (first ping only).
    pub on_options_ok: Feed,
    /// Stamped when this UAS leg's answer is confirmed (ACK received) — the
    /// shared establishment's `connected`.
    pub on_ack_rx: Feed,
    /// Stamped when this UAS leg SENDS its 200 to the initial INVITE — the
    /// refer charlie's `time_to_charlie_200` + `transferred`.
    pub on_answer_sent: Feed,
    /// Stamped when this leg RECEIVES its initial (dialog-creating) INVITE — the
    /// rerouted winning leg's `rerouted` (`rerouting_prack.rs:73`).
    pub on_invite_rx: Feed,
    /// Stamped when the `200` to this caller's PRACK arrives — the 100rel
    /// flows' `time_to_prack_200` + `pracked`.
    pub on_prack_ok: Feed,
    /// Stamped when a 2xx to this leg's sent REFER arrives — the refer bob's
    /// `time_to_202` + `referred`.
    pub on_refer_accepted: Feed,
    /// Stamped when the 200 to this leg's own BYE arrives — the shared
    /// teardown's `time_to_bye_200` + `bye_200`.
    pub on_bye_ok: Feed,
}

/// A ring/answer scheduled for `at` — held as its own interruptible arm so a
/// CANCEL that lands before it fires answers `487` on the retained INVITE txn
/// (B6-b) instead of racing an inline sleep.
struct TimedAnswer {
    at: Instant,
    /// The pending UAS INVITE transaction — answered `200` when the timer fires
    /// OR `487` if a CANCEL arrives first.
    uas: ServerTxn,
}

/// The confirmed dialog(s) + pending INVITE transaction an endpoint owns.
/// Deliberately minimal (one caller INVITE, one confirmed dialog — enough for
/// every current body incl. the realign flows, which ride the confirmed dialog
/// as in-dialog UAS transactions); the P3 ports fold the reliable-183 /
/// `Reject` pending-UAS holds into it (plan §3.5).
#[derive(Default)]
struct DialogTable {
    /// The caller's outgoing INVITE, awaiting its confirmation (learned from the
    /// responses the reactor feeds it via [`ClientInvite::absorb_response`]).
    pending_invite: Option<ClientInvite>,
    /// Our confirmed dialog (caller after ACK, or UAS after answering).
    confirmed: Option<Dialog>,
}

/// The declarative spec for one endpoint — what a scenario DECLARES; the runner
/// turns it into an [`ActorState`] wired to the shared observed state.
pub struct ActorSpec {
    /// The leg name (`"alice"`, `"bob"`, …) — the observed-state key.
    pub role: &'static str,
    /// The endpoint's bound agent.
    pub agent: Agent,
    /// How it answers its initial INVITE.
    pub disposition: Disposition,
    /// The media it negotiates with.
    pub media: MediaState,
    /// Its scripted goals (empty for a purely reactive callee).
    pub goals: Vec<Goal>,
    /// The agents an `Invite` goal can target, by callee role.
    pub invite_targets: Vec<(&'static str, Agent)>,
    /// Route a plan-less `Invite` goal through this address (a proxy/LB);
    /// `None` sends directly to the peer (the SUT-less toy call). An
    /// [`InvitePlan`](crate::realcall::InvitePlan)-carrying goal ignores it
    /// (the plan owns the route).
    pub via: Option<SocketAddr>,
    /// Which reactive events feed phases/checkpoints/the ringing gate — the
    /// per-body downstream contract (defaults stamp nothing).
    pub feed: CtxFeed,
}

/// The live per-endpoint state driven by [`run_actor`].
pub struct ActorState<'c> {
    role: &'static str,
    agent: Agent,
    disposition: Disposition,
    media: MediaState,
    dialogs: DialogTable,
    pending_answer: Option<TimedAnswer>,
    goals: GoalCursor,
    obs: ObservedState,
    scope: Arc<CallScope>,
    ctx: &'c CallCtx,
    step_timeout: Duration,
    invite_targets: HashMap<&'static str, Agent>,
    via: Option<SocketAddr>,
    feed: CtxFeed,
    /// Whether this caller has already seen (and anchored) a >100 provisional.
    saw_provisional: bool,
    /// CSeq numbers of in-dialog re-INVITEs this leg has ANSWERED (200 sent,
    /// ACK outstanding) — the matching ACK advances the realign sub-flow.
    answered_reinvites: HashSet<u32>,
    /// A reliable-`183` answer (RFC 3262) awaiting the caller's PRACK: the held
    /// UAS INVITE transaction, answered `200` only once the PRACK arrives
    /// (MUST-014). `Some` for a [`Disposition::ReliableAnswer`] leg between its
    /// 183 and the PRACK.
    pending_prack_answer: Option<ServerTxn>,
    /// RSeq values of reliable provisionals this caller has already PRACKed —
    /// so a retransmitted 183 is not double-PRACKed.
    pracked_rseqs: HashSet<u32>,
    /// This caller sent a delayed-offer re-INVITE and awaits its 2xx (which the
    /// reactor ACKs with the answer SDP). `false` for every non-`reinvite` leg.
    pending_reinvite: bool,
    /// Whether this caller has already stamped the first-OPTIONS-ping feed —
    /// so the looped `options_hold` pings stamp `keepalive_ack` exactly once.
    saw_options_200: bool,
}

impl<'c> ActorState<'c> {
    /// Wire a declarative [`ActorSpec`] to the shared observed state, teardown
    /// scope, and timing context. `step_timeout` bounds each goal-guard wait.
    pub fn from_spec(
        spec: ActorSpec,
        obs: ObservedState,
        scope: Arc<CallScope>,
        ctx: &'c CallCtx,
        step_timeout: Duration,
    ) -> Self {
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
            pracked_rseqs: HashSet::new(),
            pending_reinvite: false,
            saw_options_200: false,
        }
    }

    /// The SDP body to answer an INVITE/UPDATE with — this endpoint's answer (or
    /// offer) media, falling back to the crate default so an answer-to-INVITE is
    /// NEVER bodyless (RFC 3264 §5), even for a signalling-only endpoint
    /// ([`MediaState::none`]). A delayed-offer bodyless re-INVITE thus still gets
    /// 200 + our SDP (B6-c).
    fn answer_body(&self) -> &'static str {
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
    loop {
        tokio::select! {
            inbound = agent.recv_any() => {
                match inbound {
                    Ok(m) => default_react(&mut st, m).await?,
                    // B3: a reactor recv deadline is NOT fatal — loop again (a
                    // long_call / options_hold / the 32 s settle silence must
                    // not kill the actor). Only a closed queue is fatal.
                    Err(StepError::Timeout { .. }) => {}
                    Err(StepError::QueueClosed { .. }) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            ready = st.goals.next_ready(&obs, step_timeout), if st.goals.has_pending() => {
                let step = ready?;
                drive_goal(&mut st, step).await?;
                st.goals.advance();
            }
            _ = wait_timed_answer(&st.pending_answer), if st.pending_answer.is_some() => {
                fire_timed_answer(&mut st).await?;
            }
        }
        if obs.all_terminated() && st.goals.is_exhausted() {
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

/// Answer a UAS transaction `200 OK` with the given SDP body — the single
/// respond-200 path shared by the timed answer, the reactive re-INVITE/UPDATE
/// arm, and the immediate-answer disposition. Always carries SDP on an
/// answer-to-INVITE/UPDATE (never a bodyless 200, RFC 3264 §5); callers pass the
/// media-resolved answer via [`ActorState::answer_body`].
async fn respond_200_sdp(uas: &mut ServerTxn, sdp: &str) -> Result<(), StepError> {
    uas.respond(200, "OK").with_sdp(sdp).try_send().await
}

/// Answer a dialog-creating INVITE `200` + SDP: confirm the UAS dialog, seed
/// the dialog's received-CSeq baseline (so the first in-dialog request is not a
/// phantom hole, §12.2.1.1), open the answered-awaiting-ACK obligation (the 2xx
/// must be ACKed — the mux/SUT retransmit heals a dropped leg, and the settle
/// barrier holds the verdict until it does), and stamp the declared
/// answer-sent feed. Shared by the timed answer and the immediate disposition.
async fn answer_initial_invite(st: &mut ActorState<'_>, mut uas: ServerTxn) -> Result<(), StepError> {
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
    let sdp = st.answer_body();
    respond_200_sdp(&mut uas, sdp).await?;
    let now = Instant::now();
    st.dialogs.confirmed = Some(uas.dialog());
    st.obs.record(Observation::SeedDialog { leg: st.role, call_id, cseq }, now);
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
            detail: "answered 2xx awaiting ACK".to_string(),
        },
        now,
    );
    st.feed.on_answer_sent.stamp(st.ctx);
    Ok(())
}

/// Fire a due timed answer: `200` + the answer SDP on the retained INVITE txn,
/// then confirm the UAS dialog. (Called only when `pending_answer` is `Some`.)
async fn fire_timed_answer(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let Some(ta) = st.pending_answer.take() else {
        return Ok(());
    };
    answer_initial_invite(st, ta.uas).await
}

/// The reactive answer policy — dispatch one inbound message. Extracted and
/// generalized from the answer table in
/// [`Agent::try_receive_tolerating_blocking`]: react to WHATEVER arrives (rather
/// than "expect X, tolerate the rest"), fold the observation, and NEVER emit a
/// bodyless 200 to an offer (RFC 3264 §5).
async fn default_react(st: &mut ActorState<'_>, msg: Inbound) -> Result<(), StepError> {
    match msg {
        Inbound::Request(txn) => react_request(st, txn).await,
        Inbound::Response(resp) => react_response(st, resp).await,
    }
}

async fn react_request(st: &mut ActorState<'_>, mut uas: ServerTxn) -> Result<(), StepError> {
    let method = uas.request().method.as_str().to_string();
    let is_initial_invite = method == "INVITE" && uas.request().to.tag.is_none();
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
    let now = Instant::now();

    if is_initial_invite {
        return apply_disposition(st, uas).await;
    }

    match method.as_str() {
        // An ACK completes a transaction — absorbed, never answered. It confirms
        // our UAS dialog (the peer ACKed our 2xx) and closes the matching
        // answered-awaiting-ACK obligation (the ACK's CSeq equals the INVITE's,
        // §13.2.2.4 — closing a never-opened key is a harmless no-op).
        "ACK" => {
            // An ACK that FOLLOWS a non-2xx reject (this leg already Terminated)
            // just completes that transaction on the wire — absorb it without
            // confirming the leg or stamping the `ack` anchor (that anchor is the
            // winning leg's; the rejected b-leg's reject-ACK must not claim it).
            let already_terminated = st
                .obs
                .with_snapshot(|s| s.leg(st.role).phase() == super::state::LegPhase::Terminated);
            if already_terminated {
                return Ok(());
            }
            st.obs.record(
                Observation::ResponseObserved {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                },
                now,
            );
            if st.answered_reinvites.remove(&cseq) {
                // The ACK to a realign re-INVITE we answered — the sub-flow the
                // refer `merged` barrier conjuncts over is confirmed.
                st.obs.record(
                    Observation::Subflow {
                        leg: st.role,
                        name: SUBFLOW_REALIGN,
                        to: SubflowState::Confirmed,
                    },
                    now,
                );
            } else {
                st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
                st.ctx.anchor(&st.agent, "ack", uas.request());
                st.feed.on_ack_rx.stamp(st.ctx);
            }
        }
        // A BYE tears this leg down.
        "BYE" => {
            st.ctx.anchor(&st.agent, "bye", uas.request());
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // A CANCEL for a still-ringing INVITE: 200 the CANCEL, 487 the retained
        // INVITE txn (else the peer waits Timer C and reaping hangs), terminate.
        "CANCEL" => {
            uas.respond(200, "OK").try_send().await?;
            if let Some(ta) = st.pending_answer.take() {
                let mut inv = ta.uas;
                inv.respond(487, "Request Terminated").try_send().await?;
            }
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // In-dialog non-offer requests: 200 and fold the CSeq into the dialog's
        // gap detector (all methods share the dialog CSeq space, §12.2.1.1).
        "NOTIFY" | "OPTIONS" | "INFO" | "MESSAGE" => {
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
        }
        // An in-dialog (re-)INVITE — an offer realign. Answer 200 WITH SDP; a
        // delayed-offer bodyless re-INVITE still gets 200 + our SDP (B6-c: no
        // bodyless-200 fallthrough, RFC 3264 §5). The 200 opens an
        // answered-awaiting-ACK obligation (settle holds until the peer's ACK
        // lands — the endurance failure this harness exists to expose) and
        // advances this leg's realign sub-flow; the matching ACK confirms it.
        "INVITE" => {
            st.ctx.anchor(&st.agent, "reInvite", uas.request());
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.answered_reinvites.insert(cseq);
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id: call_id.clone(), cseq }, now);
            st.obs.record(
                Observation::RequestSent {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                    detail: "realign 200 awaiting ACK".to_string(),
                },
                now,
            );
            st.obs.record(
                Observation::Subflow {
                    leg: st.role,
                    name: SUBFLOW_REALIGN,
                    to: SubflowState::Answered,
                },
                now,
            );
        }
        // An UPDATE realign (RFC 3311) — answered 200 + SDP; its own 200
        // completes it (no ACK), so no obligation opens.
        "UPDATE" => {
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
        }
        // A PRACK (RFC 3262) for our reliable 183: 200 it, then — MUST-014 — this
        // is the trigger to answer 200 to the HELD INVITE txn (the reliable
        // provisional's whole point: no 200-to-INVITE before the PRACK).
        "PRACK" => {
            st.ctx.anchor(&st.agent, "prack", uas.request());
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
            if let Some(inv_txn) = st.pending_prack_answer.take() {
                answer_initial_invite(st, inv_txn).await?;
            }
        }
        // Any other in-dialog method: a plain 200 (dialog-neutral).
        _ => {
            uas.respond(200, "OK").try_send().await?;
        }
    }
    Ok(())
}

/// Apply the endpoint's initial-INVITE disposition (B6 entry policy).
async fn apply_disposition(st: &mut ActorState<'_>, mut uas: ServerTxn) -> Result<(), StepError> {
    let now = Instant::now();
    st.ctx.anchor(&st.agent, "initialInvite", uas.request());
    // The rerouted winning leg stamps `rerouted` on receiving its INVITE
    // (default NONE for every other body — see `CtxFeed::on_invite_rx`).
    st.feed.on_invite_rx.stamp(st.ctx);
    match st.disposition {
        // A caller should never receive an initial INVITE; answer it defensively
        // so a wiring bug doesn't strand the peer. `Answer` is the immediate
        // no-provisional answer.
        Disposition::Caller | Disposition::Answer => {
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            answer_initial_invite(st, uas).await?;
        }
        Disposition::RingThenAnswer { ring } => {
            uas.respond(180, "Ringing").try_send().await?;
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_answer = Some(TimedAnswer { at: Instant::now() + ring, uas });
        }
        Disposition::Reject(code) => {
            uas.respond(code, reject_reason(code)).try_send().await?;
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // RFC 3262: answer RELIABLY with a 183 (Require:100rel + RSeq:1 + the
        // answer SDP) and HOLD the INVITE txn — the 200 to the INVITE waits for
        // the PRACK (MUST-014, fired from the PRACK arm of `react_request`).
        Disposition::ReliableAnswer => {
            let sdp = st.answer_body();
            uas.respond(183, "Session Progress").reliable(1).with_sdp(sdp).try_send().await?;
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_prack_answer = Some(uas);
        }
    }
    Ok(())
}

async fn react_response(st: &mut ActorState<'_>, resp: SipResponse) -> Result<(), StepError> {
    let now = Instant::now();
    // A response to our still-pending caller INVITE drives the establish flow —
    // but ONLY a response whose CSeq method is INVITE. A PRACK's 200 (or any
    // other in-dialog final) sharing the early dialog must NOT be fed to the
    // INVITE transaction (`absorb_response` would misread a PRACK 200 as the
    // INVITE being answered); it falls through to the obligation-closing path.
    if resp.cseq.method == "INVITE" {
        if let Some(mut inv) = st.dialogs.pending_invite.take() {
        match inv.absorb_response(&resp).await? {
            InviteResponseFate::Provisional { status } => {
                // A 100 Trying is transaction plumbing, not an early dialog.
                if status > 100 {
                    st.obs.record(Observation::LegEarly { leg: st.role }, now);
                    if !st.saw_provisional {
                        st.saw_provisional = true;
                        st.ctx.anchor(&st.agent, "firstProvisional", &resp);
                        // The abandon body's `time_to_180` (default NONE on every
                        // other body).
                        st.feed.on_provisional.stamp(st.ctx);
                        if st.feed.ringing_gate {
                            st.ctx.mark_ringing(true);
                        }
                    }
                    // A RELIABLE provisional (RFC 3262: carries `RSeq`) must be
                    // PRACKed — once per RSeq (a retransmitted 183 is not
                    // double-PRACKed). The PRACK opens a "awaiting 200" ledger
                    // obligation the settle barrier holds on.
                    if let Some(rseq) = reliable_rseq(&resp) {
                        if st.pracked_rseqs.insert(rseq) {
                            let (_txn, req) = inv.try_prack_with_request(&resp).await?;
                            st.obs.record(
                                Observation::RequestSent {
                                    key: ObligationKey::new(
                                        st.role,
                                        ObligationKind::Prack,
                                        req.cseq.seq,
                                    ),
                                    detail: "prack awaiting 200".to_string(),
                                },
                                now,
                            );
                        }
                    }
                }
                st.dialogs.pending_invite = Some(inv);
            }
            InviteResponseFate::Answered => {
                st.ctx.anchor(&st.agent, "answer", &resp);
                if st.feed.ringing_gate && !st.saw_provisional {
                    // Answered without ever ringing: a lost non-PRACK 18x is
                    // best-effort — counted into the cross-call gate, never a
                    // per-call failure (contract table §3).
                    st.ctx.mark_ringing(false);
                }
                st.feed.on_answer_rx.stamp(st.ctx);
                // ACK the 2xx then register the confirmed dialog with NO await in
                // between, so a mid-window cancellation can never leave a
                // confirmed-but-unregistered dialog (the drop-safety rule).
                let dialog = inv.ack().await;
                st.dialogs.confirmed = Some(dialog.clone());
                st.scope.set_confirmed(dialog);
                st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
            }
            InviteResponseFate::Failed { status } => {
                st.obs.record(
                    Observation::LegFinal { leg: st.role, status, reason: resp.reason.clone() },
                    now,
                );
                st.obs.record(Observation::LegTerminated { leg: st.role }, now);
                st.scope.mark_terminated();
            }
        }
        return Ok(());
        }
        // A re-INVITE 2xx with NO pending initial INVITE: this caller's own
        // delayed-offer re-INVITE (the `reinvite` body). ACK it WITH the answer
        // SDP (RFC 3264 §4 delayed offer), close the `ReInvite` obligation the
        // goal opened, advance the caller's `reneg` sub-flow (the teardown
        // barrier), and stamp the declared feed.
        if st.pending_reinvite && (200..300).contains(&resp.status) {
            st.pending_reinvite = false;
            let sdp = st.answer_body();
            if let Some(dialog) = st.dialogs.confirmed.as_mut() {
                dialog.ack(Some(sdp)).await;
            }
            let key = ObligationKey::new(st.role, ObligationKind::ReInvite, resp.cseq.seq);
            st.obs.record(Observation::ResponseObserved { key }, now);
            st.obs.record(
                Observation::Subflow { leg: st.role, name: SUBFLOW_RENEG, to: SubflowState::Confirmed },
                now,
            );
            st.feed.on_reinvite_ok.stamp(st.ctx);
            return Ok(());
        }
    }

    // Otherwise it is a final to one of our sent in-dialog requests (our BYE's
    // 200, our REFER's 202, our NOTIFY's 200, our PRACK's 200, …) — close the
    // obligation it opened and stamp the declared feed for the flow-advancing ones.
    if let Some(kind) = ObligationKind::from_cseq_method(resp.cseq.method.as_str()) {
        let key = ObligationKey::new(st.role, kind, resp.cseq.seq);
        st.obs.record(Observation::ResponseObserved { key }, now);
        if (200..300).contains(&resp.status) {
            match kind {
                ObligationKind::Bye => {
                    st.obs.record(Observation::LegTerminated { leg: st.role }, now);
                    st.scope.mark_terminated();
                    st.feed.on_bye_ok.stamp(st.ctx);
                }
                ObligationKind::Refer => {
                    st.obs.record(
                        Observation::Subflow {
                            leg: st.role,
                            name: SUBFLOW_REFER,
                            to: SubflowState::Answered,
                        },
                        now,
                    );
                    st.feed.on_refer_accepted.stamp(st.ctx);
                }
                // The 200 to our PRACK — the 100rel flows' `pracked` /
                // `time_to_prack_200` (the reliable provisional is acknowledged).
                ObligationKind::Prack => st.feed.on_prack_ok.stamp(st.ctx),
                // The 200 to our in-dialog UPDATE — the `prack_update` flow's
                // `updated` / `time_to_update_200` (no ACK; the 200 completes it).
                // Advance the caller's `reneg` sub-flow so the teardown barrier
                // holds before the BYE.
                ObligationKind::Update => {
                    st.obs.record(
                        Observation::Subflow {
                            leg: st.role,
                            name: SUBFLOW_RENEG,
                            to: SubflowState::Confirmed,
                        },
                        now,
                    );
                    st.feed.on_update_ok.stamp(st.ctx);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The `RSeq` of a reliable provisional (RFC 3262) — `Some(rseq)` iff `resp`
/// carries a parseable `RSeq` header (marking it PRACK-required), else `None`.
fn reliable_rseq(resp: &SipResponse) -> Option<u32> {
    sip_message::message_helpers::get_header(&resp.headers, "rseq").and_then(|v| v.trim().parse().ok())
}

/// Drive one scripted goal step.
async fn drive_goal(st: &mut ActorState<'_>, step: GoalStep) -> Result<(), StepError> {
    match step {
        GoalStep::Invite { callee, plan } => {
            let target = st.invite_targets.get(callee).cloned().ok_or_else(|| {
                StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: format!("Invite goal has no bound target {callee:?}"),
                }
            })?;
            let mut builder = st.agent.invite(&target);
            if let Some(offer) = st.media.offer_sdp() {
                builder = builder.with_sdp(offer);
            }
            builder = match &plan {
                // The owned realization of `CallEnv::outgoing_invite` (route,
                // correlation stamp, egress rewrite) — the load/SUT path.
                Some(plan) => plan.apply(builder),
                // Plan-less: the toy-call path (optional bare proxy hop).
                None => match st.via {
                    Some(via) => builder.through(via),
                    None => builder,
                },
            };
            let call = builder.send().await;
            st.scope.set_early(call.cancel_handle());
            st.dialogs.pending_invite = Some(call);
        }
        GoalStep::Refer { refer_to, authorization } => {
            let now = Instant::now();
            let (key, dialog_clone, request) = {
                let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Refer goal with no confirmed dialog".to_string(),
                    }
                })?;
                let mut refer =
                    dialog.send_request(InDialogMethod::Refer).with_header("Refer-To", &refer_to);
                if let Some(api) = &authorization {
                    refer = refer.with_header("X-Api-Call", api);
                }
                // The 202 arrives through the reactor (recv_any) — the returned
                // transaction handle is not awaited on.
                let (_txn, request) = refer.try_send_with_request().await?;
                let key = ObligationKey::new(st.role, ObligationKind::Refer, request.cseq.seq);
                (key, dialog.clone(), request)
            };
            // The REFER's only receiver is the SUT itself (it builds the C leg),
            // so it is anchored as a SENT message on this leg's lane.
            st.ctx.anchor_sent(&st.agent, "refer", &request);
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(
                Observation::RequestSent { key, detail: "refer awaiting 202".to_string() },
                now,
            );
        }
        // A delayed-offer (bodyless) re-INVITE: send it, open the ReInvite
        // obligation keyed on its CSeq; the reactor ACKs the 2xx with the answer
        // SDP and stamps `on_reinvite_ok` (see `react_response`).
        GoalStep::Reinvite => {
            let now = Instant::now();
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Reinvite goal with no confirmed dialog".to_string(),
                    }
                })?;
                let _reinv = dialog.request(InDialogMethod::Invite, None).await;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::ReInvite, cseq), dialog.clone())
            };
            st.pending_reinvite = true;
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(
                Observation::RequestSent { key, detail: "re-INVITE awaiting 2xx".to_string() },
                now,
            );
        }
        // An in-dialog UPDATE (RFC 3311) carrying this leg's offer: send it, open
        // the Update obligation; its 200 closes it (no ACK) and stamps
        // `on_update_ok` (see `react_response`).
        GoalStep::Update => {
            let now = Instant::now();
            let offer = st.media.offer_sdp().unwrap_or(crate::OFFER_SDP);
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Update goal with no confirmed dialog".to_string(),
                    }
                })?;
                let _upd = dialog.send_request(InDialogMethod::Update).with_sdp(offer).try_send().await?;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Update, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(
                Observation::RequestSent { key, detail: "update awaiting 200".to_string() },
                now,
            );
        }
        // One in-dialog OPTIONS keepalive ping, its 200 read inline (the reactor
        // has nothing else to do for this leg during the ping).
        GoalStep::Options => {
            ping_options_once(st).await?;
        }
        // The OPTIONS-keepalive hold loop: ping every `cadence` until `hold`
        // elapses. Each 200 is read inline; the first stamps `keepalive_ack`.
        GoalStep::EveryOptions { cadence, hold } => {
            let start = Instant::now();
            while start.elapsed() < hold {
                tokio::time::sleep(cadence).await;
                ping_options_once(st).await?;
            }
        }
        // CANCEL the still-pending initial INVITE (RFC 3261 §9.1). KEEP the
        // pending INVITE so its `487` still routes to it (→ `Failed{487}` →
        // LegTerminated); the peer's CANCEL→200+487 is handled reactively.
        GoalStep::Cancel => {
            if let Some(inv) = st.dialogs.pending_invite.as_ref() {
                let _cxl = inv.cancel().await;
            }
        }
        GoalStep::Bye => {
            let now = Instant::now();
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Bye goal with no confirmed dialog".to_string(),
                    }
                })?;
                // Send the BYE; the reactor observes its 200 and closes the
                // obligation (we do not block on the final here).
                let _bye = dialog.bye().await;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
    }
    Ok(())
}

/// Send ONE in-dialog OPTIONS keepalive ping on the confirmed dialog and read
/// its 200 inline — the reactor is parked on the goal arm during the ping, so
/// the 200 is consumed here (mirrors the linear `options_hold`/`long_call`
/// pings). The FIRST ping stamps the `keepalive_ack` feed exactly once.
async fn ping_options_once(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let (mut opt, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Options goal with no confirmed dialog".to_string(),
        })?;
        let opt = dialog.request(InDialogMethod::Options, None).await;
        (opt, dialog.clone())
    };
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    opt.try_expect(200).await?;
    if !st.saw_options_200 {
        st.saw_options_200 = true;
        st.feed.on_options_ok.stamp(st.ctx);
    }
    Ok(())
}

/// The stock reason phrase for a rejection disposition's status code.
fn reject_reason(code: u16) -> &'static str {
    match code {
        486 => "Busy Here",
        603 => "Decline",
        487 => "Request Terminated",
        480 => "Temporarily Unavailable",
        _ => "Error",
    }
}
