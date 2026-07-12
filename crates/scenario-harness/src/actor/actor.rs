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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::goals::{Goal, GoalCursor, GoalStep};
use super::ledger::{ObligationKey, ObligationKind};
use super::state::{Observation, ObservedState};
use sip_message::SipResponse;

use crate::agent::InviteResponseFate;
use crate::realcall::{CallCtx, CallScope};
use crate::{Agent, ClientInvite, Dialog, Inbound, ServerTxn, StepError};

/// How an endpoint answers the INITIAL (dialog-creating) INVITE it receives —
/// the endpoint state machine's entry policy (B6). Later in-dialog traffic is
/// always handled reactively by [`default_react`], regardless of disposition.
#[derive(Debug, Clone, Copy)]
pub enum Disposition {
    /// Originates the call; never answers an initial INVITE.
    Caller,
    /// Answers immediately with `200` + the answer SDP.
    Answer,
    /// Rings (`180`) then answers `200` after `ring` — an interruptible timed
    /// answer (a CANCEL mid-ring yields `487`, not a stuck answer).
    RingThenAnswer { ring: Duration },
    /// Rejects the initial INVITE with a final `code` (486/603/…).
    Reject(u16),
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
/// Deliberately minimal for P0 (one caller INVITE, one confirmed dialog); P1
/// generalizes it to the multi-dialog realign flows.
///
/// Per plan §3.5 the table also holds **pending UAS `ServerTxn`s**. In P0 the
/// one such case — a ring-then-answer INVITE awaiting its scheduled 200 (or a
/// CANCEL→487) — lives in [`ActorState::pending_answer`]'s `TimedAnswer.uas`,
/// which is that pending UAS transaction. P1 folds it (and the reliable-183 /
/// `Reject` holds) into this table proper.
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
    /// Route the initial INVITE through this address (a proxy/LB); `None` sends
    /// directly to the peer (the SUT-less toy call).
    pub via: Option<SocketAddr>,
}

/// The live per-endpoint state driven by [`run_actor`].
pub struct ActorState {
    role: &'static str,
    agent: Agent,
    disposition: Disposition,
    media: MediaState,
    dialogs: DialogTable,
    pending_answer: Option<TimedAnswer>,
    goals: GoalCursor,
    obs: ObservedState,
    scope: Arc<CallScope>,
    ctx: Arc<CallCtx>,
    recv_timeout: Duration,
    invite_targets: HashMap<&'static str, Agent>,
    via: Option<SocketAddr>,
}

impl ActorState {
    /// Wire a declarative [`ActorSpec`] to the shared observed state, teardown
    /// scope, and timing context.
    pub fn from_spec(
        spec: ActorSpec,
        obs: ObservedState,
        scope: Arc<CallScope>,
        ctx: Arc<CallCtx>,
        recv_timeout: Duration,
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
            recv_timeout,
            invite_targets: spec.invite_targets.into_iter().collect(),
            via: spec.via,
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
pub async fn run_actor(mut st: ActorState) -> Result<(), StepError> {
    // Clone the Arc-backed handles the reactor arm needs, so the goal / timed
    // arms can borrow disjoint fields of `st` in the same `select!`.
    let agent = st.agent.clone();
    let obs = st.obs.clone();
    let recv_timeout = st.recv_timeout;
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
            ready = st.goals.next_ready(&obs, recv_timeout), if st.goals.has_pending() => {
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

/// Fire a due timed answer: `200` + the answer SDP on the retained INVITE txn,
/// then confirm the UAS dialog. (Called only when `pending_answer` is `Some`.)
async fn fire_timed_answer(st: &mut ActorState) -> Result<(), StepError> {
    let Some(ta) = st.pending_answer.take() else {
        return Ok(());
    };
    let mut uas = ta.uas;
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
    let sdp = st.answer_body();
    respond_200_sdp(&mut uas, sdp).await?;
    st.dialogs.confirmed = Some(uas.dialog());
    // Seed the dialog's received-CSeq baseline with the INVITE's CSeq so the
    // first in-dialog request is not a phantom hole (§12.2.1.1). Leg stays Early
    // until the ACK confirms it.
    st.obs.record(Observation::SeedDialog { leg: st.role, call_id, cseq }, Instant::now());
    st.ctx.phase("answered");
    Ok(())
}

/// The reactive answer policy — dispatch one inbound message. Extracted and
/// generalized from the answer table in
/// [`Agent::try_receive_tolerating_blocking`]: react to WHATEVER arrives (rather
/// than "expect X, tolerate the rest"), fold the observation, and NEVER emit a
/// bodyless 200 to an offer (RFC 3264 §5).
async fn default_react(st: &mut ActorState, msg: Inbound) -> Result<(), StepError> {
    match msg {
        Inbound::Request(txn) => react_request(st, txn).await,
        Inbound::Response(resp) => react_response(st, resp).await,
    }
}

async fn react_request(st: &mut ActorState, mut uas: ServerTxn) -> Result<(), StepError> {
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
        // our UAS dialog (the peer ACKed our 2xx).
        "ACK" => {
            st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
            st.ctx.phase("connected");
        }
        // A BYE tears this leg down.
        "BYE" => {
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
            st.ctx.phase("bye_200");
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
        // An in-dialog (re-)INVITE / UPDATE — an offer realign. Answer 200 WITH
        // SDP; a delayed-offer bodyless re-INVITE still gets 200 + our SDP (B6-c:
        // no bodyless-200 fallthrough, RFC 3264 §5). `answer_body` guarantees a
        // non-bodyless answer even for a signalling-only endpoint.
        "INVITE" | "UPDATE" => {
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq }, now);
        }
        // Any other in-dialog method: a plain 200 (dialog-neutral).
        _ => {
            uas.respond(200, "OK").try_send().await?;
        }
    }
    Ok(())
}

/// Apply the endpoint's initial-INVITE disposition (B6 entry policy).
async fn apply_disposition(st: &mut ActorState, mut uas: ServerTxn) -> Result<(), StepError> {
    let now = Instant::now();
    match st.disposition {
        // A caller should never receive an initial INVITE; answer it defensively
        // so a wiring bug doesn't strand the peer (P0: never exercised).
        Disposition::Caller | Disposition::Answer => {
            let call_id = uas.request().call_id.clone();
            let cseq = uas.request().cseq.seq;
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.dialogs.confirmed = Some(uas.dialog());
            st.obs.record(Observation::SeedDialog { leg: st.role, call_id, cseq }, now);
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
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
    }
    Ok(())
}

async fn react_response(st: &mut ActorState, resp: SipResponse) -> Result<(), StepError> {
    let now = Instant::now();
    // A response to our still-pending caller INVITE drives the establish flow.
    // Take the transaction out; put it back only while it stays pending.
    if let Some(mut inv) = st.dialogs.pending_invite.take() {
        match inv.absorb_response(&resp).await? {
            InviteResponseFate::Provisional { .. } => {
                st.dialogs.pending_invite = Some(inv);
                st.obs.record(Observation::LegEarly { leg: st.role }, now);
                st.ctx.mark_ringing(true);
            }
            InviteResponseFate::Answered => {
                // ACK the 2xx then register the confirmed dialog with NO await in
                // between, so a mid-window cancellation can never leave a
                // confirmed-but-unregistered dialog (the drop-safety rule).
                let dialog = inv.ack().await;
                st.dialogs.confirmed = Some(dialog.clone());
                st.scope.set_confirmed(dialog);
                st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
                st.ctx.phase("connected");
            }
            InviteResponseFate::Failed { .. } => {
                st.obs.record(Observation::LegTerminated { leg: st.role }, now);
                st.scope.mark_terminated();
            }
        }
        return Ok(());
    }

    // Otherwise it is a final to one of our sent in-dialog requests (our BYE's
    // 200, our NOTIFY's 200, …) — close the obligation it opened.
    if let Some(kind) = ObligationKind::from_cseq_method(resp.cseq.method.as_str()) {
        let key = ObligationKey::new(st.role, kind, resp.cseq.seq);
        st.obs.record(Observation::ResponseObserved { key }, now);
        if resp.cseq.method.as_str() == "BYE" && (200..300).contains(&resp.status) {
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
            st.ctx.phase("bye_200");
        }
    }
    Ok(())
}

/// Drive one scripted goal step.
async fn drive_goal(st: &mut ActorState, step: GoalStep) -> Result<(), StepError> {
    match step {
        GoalStep::Invite { callee } => {
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
            if let Some(via) = st.via {
                builder = builder.through(via);
            }
            let call = builder.send().await;
            st.scope.set_early(call.cancel_handle());
            st.dialogs.pending_invite = Some(call);
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
