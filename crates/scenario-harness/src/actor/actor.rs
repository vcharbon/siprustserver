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

use super::goals::{BodyExpect, EarlyId, FinalAssert, Goal, GoalCursor, GoalStep, RequestKind};
use super::ledger::{ObligationKey, ObligationKind};
use super::state::{Observation, ObservedState, ResponseFact, SubflowState};
use sip_message::generators::InDialogMethod;
use sip_message::{EmitOpts, MatchOpts, MessageTemplate, SipMessage, SipResponse};

use crate::agent::{top_via_branch, InviteResponseFate};
use crate::realcall::{CallCtx, CallScope, ChallengeResponder};
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
/// The sub-flow a CALLER advances once it has PRACKed the reliable provisional —
/// the observed "the early dialog exists AND is acknowledged" fact an early
/// UPDATE (C5, RFC 3311 §5.1) gates on. Distinct from `LegPhase::Early`, which
/// a caller reaches the instant she originates (before any provisional), so it
/// cannot mean "the reliable 183 is in".
pub const SUBFLOW_EARLY: &str = "early_pracked";

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
    /// Rings (`180`) then stays SILENT forever — the ring-then-timeout stimulus
    /// a NO-ANSWER-triggered failover needs (newkahneed-047): the INVITE server
    /// transaction is held open so the SUT's OWN no-answer timer is what ends
    /// the leg. The SUT's timer-driven CANCEL yields `487` (the same held-txn
    /// path as a mid-ring CANCEL), so the leg settles cleanly under the reroute
    /// with no stuck obligation or leaked server txn.
    RingThenSilent,
    /// Rejects the initial INVITE with a final `code` (486/603/…).
    Reject(u16),
    /// Answers RELIABLY (RFC 3262): a `183` carrying `Require:100rel` + `RSeq` +
    /// the answer SDP, then HOLDS the INVITE transaction, answering `200` to the
    /// INVITE only after the caller PRACKs (MUST-014 ordering). The
    /// rerouting/prack winning-leg disposition.
    ReliableAnswer,
    /// Like [`ReliableAnswer`](Self::ReliableAnswer) but HOLDS the `200` to the
    /// INVITE until an EARLY UPDATE has been answered (C5, RFC 3311 §5.1): 183
    /// reliable → PRACK (200'd, INVITE still held) → UPDATE (200'd) → THEN the
    /// final 200 INVITE. The callee for an early-UPDATE (`Script::UpdateEarly`)
    /// establishment, where the caller renegotiates media on the early dialog
    /// before the call is answered.
    ReliableAnswerEarlyUpdate,
    /// A **forking UAS** (C1/E3, RFC 3261 §12.1.2): emits one 18x per tag in
    /// `tags` — DISTINCT explicit To-tags on the ONE retained INVITE server
    /// transaction, as if a proxy downstream had forked — then answers `200`
    /// under the `winner` tag. `reliable: false` → plain `180`s and a timed
    /// answer after `ring` (a CANCEL mid-ring still yields 487, like
    /// [`RingThenAnswer`](Self::RingThenAnswer)); `reliable: true` → each fork's
    /// 18x is a reliable `183` (`Require:100rel`, `RSeq:1`, the answer SDP) and
    /// the `200` waits for the WINNER fork's PRACK (`ring` is unused).
    /// `loser_late_200: Some(tag)` additionally emits a LATE `200` under that
    /// losing tag right after the winner's — the §13.2.2.4 loser the caller
    /// must ACK then BYE. `winner` (and the late-200 loser, distinct from the
    /// winner) must be members of `tags` — enforced at INVITE time.
    ForkingRing {
        tags: &'static [&'static str],
        winner: &'static str,
        ring: Duration,
        reliable: bool,
        loser_late_200: Option<&'static str>,
    },
    /// Never auto-answers by policy. Inbound requests PARK on a per-actor
    /// queue when a remaining scripted goal will consume/answer them; anything
    /// the script never consumes falls through to the reactive core (recorded
    /// as a serviced stray) — peers stay RFC-compliant when the SUT relays
    /// traffic the script never modeled.
    Scripted,
}

/// Per-plan (lane-chosen) stack automatics for scripted endpoints. When set, an
/// inbound INVITE parked on a [`Disposition::Scripted`] actor is answered
/// `100 Trying` immediately (RFC 3261 §17.2.1) — identically on every lane; the
/// `100` never consumes the transaction.
#[derive(Debug, Clone, Copy, Default)]
pub struct Automatics {
    pub answer_100_trying: bool,
}

/// One inbound request parked on a [`Disposition::Scripted`] actor, awaiting
/// the reception goal that consumes it (or a requeue-on-advance auto-react).
struct ParkedRequest {
    txn: ServerTxn,
    /// Whether this is the dialog-creating INVITE (no To-tag).
    initial: bool,
}

/// The forking-UAS answer plan carried from the 18x emission to the moment the
/// INVITE is answered (the timed-answer arm, or the winner fork's PRACK): the
/// `200` goes out under `winner_tag` (adopted as the transaction's dialog tag),
/// then optionally a LATE `200` under `loser_late_200`.
#[derive(Debug, Clone, Copy)]
struct ForkAnswer {
    winner_tag: &'static str,
    loser_late_200: Option<&'static str>,
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
    /// A forking callee's answer plan (`None` for the plain ring→answer): the
    /// `200` goes out under the winning fork's tag (+ optional loser late 200).
    fork: Option<ForkAnswer>,
}

/// A non-2xx final we sent to an initial INVITE, awaiting its §17.1.1.3
/// hop-ACK. The receive core claims that ACK below `recv_any` (it never
/// surfaces to the reactor — 036 ask B), so the actor watches the transaction
/// layer's fulfilment as its own `select!` arm and closes the `reject-final`
/// ledger obligation there. This is what makes the UA outlive the call: a
/// REJECTED leg abandoned by a reroute keeps its actor reactive (and the
/// settle barrier holds the verdict + recording window, Timer-H-bounded) until
/// a lost hop-ACK is recovered by the Timer-G final retransmit + the SUT's
/// §17.1.1.2 re-ACK.
struct PendingRejectAck {
    key: ObligationKey,
    call_id: String,
    branch: String,
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
    /// The caller's establishing INVITE, RETAINED after confirmation (C1/E3):
    /// a LOSING fork's late 2xx (§13.2.2.4) arrives after the winner's and must
    /// be ACK+BYE'd on ITS OWN fork dialog — derived from this transaction
    /// ([`ClientInvite::fork_dialog`]), never from the confirmed (winner) one.
    won_invite: Option<ClientInvite>,
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
    /// A [`Disposition::RingThenSilent`] leg's held INVITE server transaction
    /// (180 sent, no final EVER originated by this leg): released only by an
    /// inbound CANCEL — the SUT's no-answer timer firing — which 487s it
    /// (newkahneed-047).
    held_silent: Option<ServerTxn>,
    /// `(fork To-tag, RSeq)` pairs of reliable provisionals this caller has
    /// already PRACKed — so a retransmitted 183 is not double-PRACKed, while
    /// a FORKED reliable 183 (distinct tag, same RSeq space per §12.1.2 — each
    /// fork typically starts at RSeq 1) still gets its OWN PRACK (C1/E3).
    pracked_rseqs: HashSet<(String, u32)>,
    /// A held forking-UAS answer plan (C1/E3): set alongside
    /// `pending_prack_answer` by a RELIABLE [`Disposition::ForkingRing`], so the
    /// PRACK arm answers the INVITE only on the WINNER fork's PRACK (a losing
    /// fork's PRACK is 200'd but does not release the 200-to-INVITE).
    fork_answer: Option<ForkAnswer>,
    /// The LOSING fork tags this forking callee emitted a late `200` under — an
    /// inbound BYE addressed to one of these tears down only that early fork,
    /// NOT this leg (the winning dialog lives on; the BYE is 200'd and its CSeq
    /// folded into the dialog stream, but no `LegTerminated` is recorded).
    fork_loser_tags: HashSet<String>,
    /// CSeq numbers of in-dialog re-INVITEs THIS caller has ORIGINATED whose
    /// `reneg` sub-flow has not yet been advanced (the `reinvite` body's
    /// delayed-offer re-INVITE). A set keyed by CSeq — NOT a one-shot bool — so
    /// the 2xx ACK is re-derivable and a lost datagram interleaving can never
    /// strand it (mirrors the mux's `(Call-ID, CSeq)` re-ACK). Empty for every
    /// non-`reinvite` leg.
    sent_reinvites: HashSet<u32>,
    /// The client transaction handle of each outstanding originated re-INVITE,
    /// keyed by CSeq — retained so a NON-2xx final (a `491 Request Pending`
    /// glare reject, C4/S5) can be hop-ACKed (§17.1.1.3): `recv_any` surfaces
    /// the 491 as a bare response without auto-ACKing it. Cleared in lockstep
    /// with `sent_reinvites` on the 2xx OR the 491.
    sent_reinvite_txns: HashMap<u32, crate::InDialogTxn>,
    /// A pending §14.1 re-INVITE glare RETRY (C4/S5): set when our re-INVITE
    /// drew a 491, fires after the owner/non-owner dwell to re-originate it.
    reinvite_retry: Option<Instant>,
    /// CSeq numbers of in-dialog UPDATEs we originated awaiting their 200 — an
    /// OUTSTANDING OFFER (RFC 3311 §5.1). Both this and `sent_reinvites`
    /// represent an outstanding offer, so an incoming offer-bearing UPDATE or
    /// re-INVITE while EITHER is non-empty is 491'd (C4/S6 collision).
    sent_updates: HashSet<u32>,
    /// A pending UPDATE-collision RETRY (C4/S6): set when our UPDATE drew a 491,
    /// fires after the §14.1-style back-off to re-originate it (UPDATE has no
    /// ACK, so the 491 alone completes its transaction).
    update_retry: Option<Instant>,
    /// C5: a [`Disposition::ReliableAnswerEarlyUpdate`] callee HOLDS the INVITE
    /// 200 across the PRACK, answering it only after an early UPDATE is 200'd
    /// (RFC 3311 §5.1). `true` only for that disposition.
    hold_for_early_update: bool,
    /// C5 ordering: whether this early-UPDATE callee has PRACKed its reliable
    /// 183 (RFC 3262 MUST-014 — the 2xx must not precede the PRACK) and whether
    /// the early UPDATE has been 200'd. The held INVITE 200 is released only
    /// once BOTH hold, so a UPDATE that races ahead of the PRACK does not answer
    /// the INVITE early.
    early_pracked: bool,
    early_updated: bool,
    /// Whether this caller has already stamped the first-OPTIONS-ping feed —
    /// so the looped `options_hold` pings stamp `keepalive_ack` exactly once.
    saw_options_200: bool,
    /// The provisional this caller's establishing INVITE awaits — `180` by
    /// default, `183` once it has advertised `Supported: 100rel` (the reliable
    /// flows). The `expected` field of the incidental `WrongStatus` a shed/reject
    /// on the establishing INVITE surfaces (linear `establish`/`establish_100rel`
    /// parity).
    expected_provisional: u16,
    /// A sent non-2xx initial-INVITE final whose hop-ACK is outstanding — its
    /// own `select!` arm closes the `reject-final` obligation on fulfilment.
    /// At most one per leg (a leg rejects its initial INVITE once, or 487s its
    /// one CANCELled ring).
    pending_reject_ack: Option<PendingRejectAck>,
    /// The **deferred-auth adapter** (RFC 3261 §22.2) wired onto this caller's
    /// establishing INVITE. `Some` → a `401`/`407` to that INVITE is ACKed, the
    /// responder is asked for a credential, and the INVITE is resent ONCE
    /// (bumped CSeq, fresh branch — see [`ClientInvite::ack_and_resend_with_auth`]);
    /// `None` (the default) → a challenge classifies as `status_401/407`
    /// unchanged. Reaches the caller from [`CallEnv::challenge_responder`]
    /// (`crates/loadgen/src/driver.rs` `run_one`).
    challenge_responder: Option<Arc<dyn ChallengeResponder>>,
    /// Authenticated INVITE resends still permitted (RFC 3261 §22.2) — `1` when a
    /// [`challenge_responder`](Self::challenge_responder) is wired, else `0`.
    /// Capped so a challenge to the *resent* INVITE surfaces as a plain
    /// `status_401/407` deviation, never an unbounded loop.
    auth_retries_left: u8,
    /// A [`Disposition::Scripted`] actor's parked inbound requests, in arrival
    /// order — reception goals consume them; requeue-on-advance auto-reacts
    /// what no remaining goal can consume.
    parked: Vec<ParkedRequest>,
    /// The automatic that consumed the parked initial INVITE (CANCEL → 487): a
    /// later scripted step bound to it fails fast naming this, never by timeout.
    parked_initial_consumed: Option<&'static str>,
    /// The server transaction the nearest preceding `ExpectRequest` consumed —
    /// what a `RespondTemplate`/`Respond` goal answers.
    bound: Option<ServerTxn>,
    /// This actor's cursor into its leg's ordered response-fact log — the
    /// consumption point of the reception goals.
    resp_seen: usize,
    /// The ACK body resolved for each in-dialog INVITE 2xx we ACKed, keyed by
    /// CSeq — the ACK to a RETRANSMITTED 2xx must be byte-identical
    /// (RFC 3261 §13.2.2.4), so an `ack_body` override is resolved once and
    /// re-emitted verbatim, never re-derived from the (advanced) goal cursor.
    reinvite_ack_bodies: HashMap<u32, String>,
    /// The plan's lane-chosen stack automatics.
    automatics: Automatics,
    /// Whether this actor ORIGINATES the dialog — its first goal is an
    /// `Invite`/`InviteTemplate` (fallback: `Disposition::Caller`). Keys the
    /// §14.1 glare owner dwell and the caller attribution.
    originates: bool,
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
        challenge_responder: Option<Arc<dyn ChallengeResponder>>,
        automatics: Automatics,
    ) -> Self {
        let originates = spec
            .goals
            .first()
            .is_some_and(|g| {
                matches!(g.step, GoalStep::Invite { .. } | GoalStep::InviteTemplate { .. })
            })
            || matches!(spec.disposition, Disposition::Caller);
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
            reinvite_ack_bodies: HashMap::new(),
            automatics,
            originates,
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

/// Open the `reject-final awaiting hop-ACK` ledger obligation for a non-2xx
/// final just sent on `uas` (§17.2.1: a real UAS keeps its server txn — and
/// this harness its recording window — until the final is ACKed, even when the
/// call has moved on). Skipped for a branch-less request, exactly like the
/// agent's own ACK-obligation table (nothing to match the hop-ACK by).
fn arm_reject_final(st: &mut ActorState<'_>, uas: &ServerTxn, code: u16) {
    let Some(branch) = top_via_branch(&uas.request().headers) else { return };
    let call_id = uas.request().call_id.clone();
    let key = ObligationKey::new(st.role, ObligationKind::RejectFinal, uas.request().cseq.seq);
    st.obs.record(
        Observation::RequestSent { key, detail: format!("{code} final awaiting hop-ACK") },
        Instant::now(),
    );
    st.pending_reject_ack = Some(PendingRejectAck { key, call_id, branch });
}

/// Discharge this leg's still-open in-dialog acknowledgement obligations because
/// its DIALOG is being torn down (a BYE in either direction, §15). Any pending
/// ack — a re-INVITE we answered awaiting its ACK, a PRACK/UPDATE awaiting its
/// 200, an in-dialog request awaiting its 2xx — is MOOT once the dialog ends: the
/// far end's transaction dies with the call, so the ack can never arrive. Without
/// this the settle barrier holds the verdict its full 32 s ceiling waiting for
/// that impossible ack, surfacing under loss as a spurious `settle@…` timeout —
/// the residual re-INVITE/realign/PRACK tail where an answering/renegotiating leg
/// strands when the peer BYEs (`reinvite_gap = 0`) before a lost ack could be
/// recovered. The `RejectFinal` obligation is deliberately preserved (it outlives
/// the call on a REROUTE-abandoned leg — never a BYE — see its ledger doc). The
/// `answered_reinvites` set is drained in lockstep for hygiene.
fn discharge_on_teardown(st: &mut ActorState<'_>, now: Instant) {
    st.answered_reinvites.clear();
    st.obs.record(Observation::DialogTornDown { leg: st.role }, now);
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
///
/// A forking callee (`fork: Some`) answers under the WINNER fork's tag (adopted
/// as the txn's dialog tag, so `uas.dialog()` keys the confirmed dialog under
/// it), then optionally emits a losing fork's LATE `200` (distinct tag on the
/// same transaction — §13.2.2.4: the caller ACKs then BYEs it). Both 200s share
/// the INVITE's CSeq, so the ONE answered-awaiting-ACK obligation covers them
/// (the wire-level per-fork ACK completeness is the RFC audit's to judge).
async fn answer_initial_invite(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
    fork: Option<ForkAnswer>,
) -> Result<(), StepError> {
    let sdp = st.answer_body();
    if let Some(f) = &fork {
        uas.adopt_to_tag(f.winner_tag);
    }
    respond_200_sdp(&mut uas, sdp).await?;
    note_uas_answered(st, &uas);
    if let Some(loser) = fork.and_then(|f| f.loser_late_200) {
        // The losing fork's LATE 200 — after the winner's, under the losing
        // fork's own tag (the txn's sticky tag stays the winner's).
        uas.respond(200, "OK").with_sdp(sdp).with_to_tag(loser).try_send().await?;
    }
    st.feed.on_answer_sent.stamp(st.ctx);
    Ok(())
}

/// The bookkeeping a 2xx to a dialog-creating INVITE requires, response
/// already sent: confirm the UAS dialog, seed the received-CSeq baseline
/// (§12.2.1.1), and open the answered-awaiting-ACK obligation. Shared by the
/// policy answer and the scripted `Respond`/`RespondTemplate` finals.
fn note_uas_answered(st: &mut ActorState<'_>, uas: &ServerTxn) {
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
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
}

/// Fire a due timed answer: `200` + the answer SDP on the retained INVITE txn,
/// then confirm the UAS dialog. (Called only when `pending_answer` is `Some`.)
async fn fire_timed_answer(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let Some(ta) = st.pending_answer.take() else {
        return Ok(());
    };
    answer_initial_invite(st, ta.uas, ta.fork).await
}

/// Whether the goal arm may fire the NEXT pending goal: a reception (or
/// binding-consuming) goal additionally requires its consumable — a new
/// response fact, a matching parked request, or a bound/parked transaction
/// (a tombstoned binding enables the arm so it FAILS fast, never by timeout).
fn goal_arm_enabled(st: &ActorState<'_>) -> bool {
    match st.goals.next_step() {
        Some(GoalStep::ExpectRequest { kind, .. }) => {
            st.parked.iter().any(|p| parked_matches(p, kind))
                || (matches!(kind, RequestKind::Initial) && st.parked_initial_consumed.is_some())
        }
        Some(GoalStep::RespondTemplate { .. } | GoalStep::Respond { .. }) => {
            st.bound.is_some()
                || st.parked.iter().any(|p| p.initial)
                || st.parked_initial_consumed.is_some()
        }
        Some(GoalStep::ExpectResponse { status, .. }) => {
            let need_final = *status >= 200;
            st.obs
                .with_snapshot(|s| s.leg_response_ready(st.role, st.resp_seen, need_final))
        }
        Some(GoalStep::ObserveFinal { .. } | GoalStep::ExpectFinal { .. }) => {
            st.obs.with_snapshot(|s| s.leg_response_ready(st.role, st.resp_seen, true))
        }
        _ => true,
    }
}

/// Whether a parked request satisfies an `ExpectRequest`'s kind.
fn parked_matches(p: &ParkedRequest, kind: &RequestKind) -> bool {
    match kind {
        RequestKind::Initial => p.initial,
        RequestKind::InDialog(m) => !p.initial && p.txn.request().method.as_str() == m.as_str(),
    }
}

/// Whether a remaining scripted goal will consume/answer the parked initial
/// INVITE. Walks the remaining goals tracking the binding a `RespondTemplate`/
/// `Respond` would take at that point: an `ExpectRequest{Initial}` always
/// parks; a respond parks only when NO in-dialog `ExpectRequest` binding is
/// pending before it (else it answers that bound request, not the initial) —
/// so a stray initial never over-parks behind an in-dialog script tail.
fn scripted_wants_initial(st: &ActorState<'_>) -> bool {
    let mut bound_pending = st.bound.is_some();
    for s in st.goals.remaining_steps() {
        match s {
            GoalStep::ExpectRequest { kind: RequestKind::Initial, .. } => return true,
            GoalStep::ExpectRequest { kind: RequestKind::InDialog(_), .. } => {
                bound_pending = true;
            }
            GoalStep::RespondTemplate { template, .. } => {
                if !bound_pending {
                    return true;
                }
                // A final consumes the pending binding; a provisional keeps it.
                if template.status().is_some_and(|(s, _)| s >= 200) {
                    bound_pending = false;
                }
            }
            GoalStep::Respond { status } => {
                if !bound_pending {
                    return true;
                }
                if *status >= 200 {
                    bound_pending = false;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a remaining `ExpectRequest` will consume an in-dialog request of
/// this method.
fn scripted_wants_in_dialog(st: &ActorState<'_>, method: &str) -> bool {
    st.goals.remaining_steps().any(|s| {
        matches!(s, GoalStep::ExpectRequest { kind: RequestKind::InDialog(m), .. }
            if m.as_str() == method)
    })
}

/// Requeue-on-advance: auto-react every parked request no remaining goal can
/// consume (recorded as a serviced stray) — a parked request never starves
/// behind a script that moved past it.
async fn requeue_parked(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let mut i = 0;
    while i < st.parked.len() {
        let keep = if st.parked[i].initial {
            scripted_wants_initial(st)
        } else {
            let method = st.parked[i].txn.request().method.as_str().to_string();
            scripted_wants_in_dialog(st, &method)
        };
        if keep {
            i += 1;
            continue;
        }
        let entry = st.parked.remove(i);
        let method = entry.txn.request().method.as_str().to_string();
        st.obs.record(
            Observation::ServicedStray { leg: st.role, method, action: "auto-reacted on advance" },
            now,
        );
        if entry.initial {
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            answer_initial_invite(st, entry.txn, None).await?;
        } else {
            react_in_dialog_request(st, entry.txn).await?;
        }
    }
    Ok(())
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

async fn react_request(st: &mut ActorState<'_>, uas: ServerTxn) -> Result<(), StepError> {
    let method = uas.request().method.as_str().to_string();
    let is_initial_invite = method == "INVITE" && uas.request().to.tag.is_none();

    if is_initial_invite {
        return apply_disposition(st, uas).await;
    }

    // A Scripted actor's in-dialog park-or-react (ACK and CANCEL are stack
    // automatics, never parked): a request a remaining `ExpectRequest` matches
    // waits for the script; anything else falls through to the reactive core,
    // recorded as a serviced stray so divergence is never silent.
    if matches!(st.disposition, Disposition::Scripted) && method != "ACK" && method != "CANCEL" {
        let now = Instant::now();
        if scripted_wants_in_dialog(st, &method) {
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: uas.request().call_id.clone(),
                    cseq: uas.request().cseq.seq,
                    method,
                },
                now,
            );
            st.parked.push(ParkedRequest { txn: uas, initial: false });
            return Ok(());
        }
        st.obs.record(
            Observation::ServicedStray {
                leg: st.role,
                method: method.clone(),
                action: "auto-reacted",
            },
            now,
        );
    }
    react_in_dialog_request(st, uas).await
}

/// The reactive in-dialog answer table — shared by the live dispatch and the
/// requeue-on-advance auto-react (a re-recorded `InDialogRequest` observation
/// is idempotent, so re-entry for a previously parked request is harmless).
async fn react_in_dialog_request(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
) -> Result<(), StepError> {
    let method = uas.request().method.as_str().to_string();
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
    let now = Instant::now();

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
            // It DOES close whichever acknowledgement obligation it satisfies
            // (a reject-final's hop-ACK that surfaced unclaimed, or a recovered
            // answer-ACK landing after the leg was BYE-terminated) — closing a
            // never-opened key is a harmless no-op.
            let already_terminated = st
                .obs
                .with_snapshot(|s| s.leg(st.role).phase() == super::state::LegPhase::Terminated);
            if already_terminated {
                st.obs.record(
                    Observation::ResponseObserved {
                        key: ObligationKey::new(st.role, ObligationKind::RejectFinal, cseq),
                    },
                    now,
                );
                st.obs.record(
                    Observation::ResponseObserved {
                        key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                    },
                    now,
                );
                if st.pending_reject_ack.as_ref().is_some_and(|p| p.key.cseq == cseq) {
                    st.pending_reject_ack = None;
                }
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
        // A BYE tears this leg down — UNLESS it is addressed to a LOSING fork's
        // tag (C1/E3: the caller ACK+BYEs a losing fork's late 200, §13.2.2.4):
        // that BYE ends only the abandoned early fork, the winning dialog lives
        // on, so it is 200'd (and its CSeq folded into the dialog stream) but
        // the leg is NOT terminated.
        "BYE" => {
            let is_fork_teardown = uas
                .request()
                .to
                .tag
                .as_deref()
                .is_some_and(|t| st.fork_loser_tags.contains(t));
            st.ctx.anchor(&st.agent, "bye", uas.request());
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
            if is_fork_teardown {
                return Ok(());
            }
            // An in-dialog ack this leg still awaits (a re-INVITE it answered, a
            // PRACK/UPDATE, …) is moot now the dialog ends (§15) — discharge it so
            // settle does not wait 32 s for an ack the torn-down peer can never send.
            discharge_on_teardown(st, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // A CANCEL (RFC 3261 §9.2): always 200 the CANCEL. If the INVITE is
        // still PENDING (a ring not yet answered, or a reliable 183 held for its
        // PRACK), 487 the retained INVITE txn (else the peer waits Timer C and
        // reaping hangs) and terminate the leg. But if we have ALREADY answered
        // — the 200 crossed the CANCEL (C2/E5) — the CANCEL "has no effect on the
        // call" (§9.2): 200 it and IGNORE it, leaving the confirmed dialog up so
        // the caller ACKs the 200 and BYEs. NEVER terminate an already-confirmed
        // leg on a late CANCEL.
        "CANCEL" => {
            uas.respond(200, "OK").try_send().await?;
            let mut from_parked = false;
            let held = st
                .pending_answer
                .take()
                .map(|ta| ta.uas)
                .or_else(|| st.pending_prack_answer.take())
                .or_else(|| st.held_silent.take())
                // A Scripted actor's PARKED initial INVITE: the CANCEL automatic
                // consumes it (200 + 487) — a later scripted step bound to it
                // fails fast via the tombstone, never by goal timeout.
                .or_else(|| {
                    let i = st.parked.iter().position(|p| p.initial)?;
                    from_parked = true;
                    Some(st.parked.remove(i).txn)
                });
            if let Some(mut inv) = held {
                inv.respond(487, "Request Terminated").try_send().await?;
                arm_reject_final(st, &inv, 487);
                if from_parked {
                    st.parked_initial_consumed = Some("CANCEL answered 200 + 487");
                    st.obs.record(
                        Observation::ServicedStray {
                            leg: st.role,
                            method: "CANCEL".to_string(),
                            action: "200 + 487 on the parked INVITE",
                        },
                        now,
                    );
                }
                st.obs.record(Observation::LegTerminated { leg: st.role }, now);
                st.scope.mark_terminated();
            }
        }
        // In-dialog non-offer requests: 200 and fold the CSeq into the dialog's
        // gap detector (all methods share the dialog CSeq space, §12.2.1.1).
        "NOTIFY" | "OPTIONS" | "INFO" | "MESSAGE" => {
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
        }
        // An in-dialog (re-)INVITE — an offer realign. Answer 200 WITH SDP; a
        // delayed-offer bodyless re-INVITE still gets 200 + our SDP (B6-c: no
        // bodyless-200 fallthrough, RFC 3264 §5). The 200 opens an
        // answered-awaiting-ACK obligation (settle holds until the peer's ACK
        // lands — the endurance failure this harness exists to expose) and
        // advances this leg's realign sub-flow; the matching ACK confirms it.
        "INVITE" => {
            st.ctx.anchor(&st.agent, "reInvite", uas.request());
            // GLARE (C4/S5+S6, RFC 3261 §14.1 / RFC 3311 §5.2): if THIS leg has
            // its OWN offer outstanding when the peer's re-INVITE arrives — an
            // un-answered re-INVITE (S5) OR an un-answered UPDATE (S6) — reject
            // the peer's with `491 Request Pending` (never two overlapping
            // offer/answer rounds on one dialog). Hop-ACK obligation armed like
            // any non-2xx INVITE final; the peer closes it, backs off, and
            // retries. No realign sub-flow advances (the round did not complete).
            if !st.sent_reinvites.is_empty() || !st.sent_updates.is_empty() {
                uas.respond(491, "Request Pending").try_send().await?;
                arm_reject_final(st, &uas, 491);
                st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
                return Ok(());
            }
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.answered_reinvites.insert(cseq);
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id: call_id.clone(), cseq, method: method.clone() }, now);
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
        // completes it (no ACK), so no obligation opens. COLLISION (C4/S6, RFC
        // 3311 §5.2): if THIS leg has its OWN offer outstanding (a re-INVITE or
        // UPDATE we sent, un-answered), the incoming UPDATE's offer glares —
        // reject it 491. Unlike the re-INVITE 491, an UPDATE's non-2xx final
        // takes NO hop-ACK (UPDATE is a non-INVITE transaction), so no
        // reject-final obligation is armed.
        "UPDATE" => {
            if !st.sent_reinvites.is_empty() || !st.sent_updates.is_empty() {
                uas.respond(491, "Request Pending").try_send().await?;
                st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
                return Ok(());
            }
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
            // C5 (RFC 3311 §5.1): the EARLY UPDATE's offer/answer completed —
            // release the held INVITE 200, but only once the reliable 183 was
            // also PRACKed (MUST-014); if the UPDATE raced ahead of the PRACK
            // the PRACK arm releases it instead.
            if st.hold_for_early_update {
                st.early_updated = true;
                maybe_answer_held_invite(st).await?;
            }
        }
        // A PRACK (RFC 3262) for our reliable 183: 200 it, then — MUST-014 — this
        // is the trigger to answer 200 to the HELD INVITE txn (the reliable
        // provisional's whole point: no 200-to-INVITE before the PRACK). A
        // FORKING callee (C1/E3) answers only on the WINNER fork's PRACK — a
        // losing fork's PRACK (identified by its To-tag) is 200'd and absorbed.
        "PRACK" => {
            st.ctx.anchor(&st.agent, "prack", uas.request());
            let prack_tag = uas.request().to.tag.clone();
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(Observation::InDialogRequest { leg: st.role, call_id, cseq, method: method.clone() }, now);
            // C5: this callee holds the INVITE for an early UPDATE — mark the
            // 183 PRACKed and release the held 200 only once the UPDATE is also
            // done (MUST-014 + RFC 3311 §5.1, in either arrival order).
            if st.hold_for_early_update {
                st.early_pracked = true;
                maybe_answer_held_invite(st).await?;
                return Ok(());
            }
            let releases_answer = match (&st.fork_answer, prack_tag.as_deref()) {
                // Forking: only the winner fork's PRACK releases the 200.
                (Some(f), Some(tag)) => tag == f.winner_tag,
                (Some(_), None) => false,
                // Non-forking reliable answer: any PRACK releases (as before).
                (None, _) => true,
            };
            if releases_answer {
                if let Some(inv_txn) = st.pending_prack_answer.take() {
                    let fork = st.fork_answer.take();
                    answer_initial_invite(st, inv_txn, fork).await?;
                }
            }
        }
        // Any other in-dialog method: a plain 200 (dialog-neutral).
        _ => {
            uas.respond(200, "OK").try_send().await?;
        }
    }
    Ok(())
}

/// C5: release a [`Disposition::ReliableAnswerEarlyUpdate`] callee's HELD
/// INVITE 200, but only once BOTH the reliable 183 is PRACKed (RFC 3262
/// MUST-014) AND the early UPDATE is 200'd (RFC 3311 §5.1) — regardless of the
/// order those two arrive in. A no-op otherwise.
async fn maybe_answer_held_invite(st: &mut ActorState<'_>) -> Result<(), StepError> {
    if st.hold_for_early_update && st.early_pracked && st.early_updated {
        if let Some(inv_txn) = st.pending_prack_answer.take() {
            answer_initial_invite(st, inv_txn, None).await?;
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
            answer_initial_invite(st, uas, None).await?;
        }
        Disposition::RingThenAnswer { ring } => {
            uas.respond(180, "Ringing").try_send().await?;
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_answer = Some(TimedAnswer { at: Instant::now() + ring, uas, fork: None });
        }
        // Ring then SILENCE (047): the 180 goes out, the INVITE server txn is
        // held with NO answer ever scheduled — the SUT's own no-answer timer
        // must end this leg. Its CANCEL lands in `react_request`'s CANCEL arm,
        // which takes the held txn and 487s it (arming the reject-final
        // obligation).
        Disposition::RingThenSilent => {
            uas.respond(180, "Ringing").try_send().await?;
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.held_silent = Some(uas);
        }
        // C1/E3 forking UAS: one 18x per DISTINCT explicit To-tag on the ONE
        // retained INVITE server txn (as if a downstream proxy forked), then the
        // 200 under the winner's tag — timed (plain 180s) or on the winner's
        // PRACK (reliable 183s, MUST-014).
        Disposition::ForkingRing { tags, winner, ring, reliable, loser_late_200 } => {
            let wired_ok = tags.contains(&winner)
                && loser_late_200.is_none_or(|l| l != winner && tags.contains(&l));
            if !wired_ok {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "ForkingRing winner/loser must be distinct declared fork tags"
                        .to_string(),
                });
            }
            let sdp = st.answer_body();
            for tag in tags {
                if reliable {
                    // RFC 3262 §3: each fork's reliable 183 carries its own RSeq
                    // space (RSeq:1 per early dialog) + the answer SDP.
                    uas.respond(183, "Session Progress")
                        .with_to_tag(tag)
                        .reliable(1)
                        .with_sdp(sdp)
                        .try_send()
                        .await?;
                } else {
                    uas.respond(180, "Ringing").with_to_tag(tag).try_send().await?;
                }
            }
            if let Some(loser) = loser_late_200 {
                st.fork_loser_tags.insert(loser.to_string());
            }
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            let fork = ForkAnswer { winner_tag: winner, loser_late_200 };
            if reliable {
                // The 200 waits for the WINNER fork's PRACK (see the PRACK arm).
                st.fork_answer = Some(fork);
                st.pending_prack_answer = Some(uas);
            } else {
                st.pending_answer =
                    Some(TimedAnswer { at: Instant::now() + ring, uas, fork: Some(fork) });
            }
        }
        Disposition::Reject(code) => {
            uas.respond(code, reject_reason(code)).try_send().await?;
            arm_reject_final(st, &uas, code);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // RFC 3262: answer RELIABLY with a 183 (Require:100rel + RSeq:1 + the
        // answer SDP) and HOLD the INVITE txn — the 200 to the INVITE waits for
        // the PRACK (MUST-014, fired from the PRACK arm of `react_request`).
        // Both reliable-answer dispositions emit the same reliable 183 and HOLD
        // the INVITE. They differ only in WHEN the held 200 is released: the
        // plain one on the PRACK (MUST-014); the early-UPDATE one after the
        // early UPDATE is answered (C5, RFC 3311 §5.1 — see the PRACK/UPDATE arms).
        Disposition::ReliableAnswer | Disposition::ReliableAnswerEarlyUpdate => {
            let sdp = st.answer_body();
            uas.respond(183, "Session Progress").reliable(1).with_sdp(sdp).try_send().await?;
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_prack_answer = Some(uas);
        }
        // Scripted park-or-react for the initial INVITE: park when a remaining
        // goal will consume/answer it, else auto-answer 200 (the RFC-compliant
        // react default) and record the stray. The §5 automatic answers `100
        // Trying` in both cases — it never consumes the transaction.
        Disposition::Scripted => {
            if st.automatics.answer_100_trying {
                uas.respond(100, "Trying").try_send().await?;
            }
            if scripted_wants_initial(st) {
                st.parked.push(ParkedRequest { txn: uas, initial: true });
            } else {
                st.obs.record(
                    Observation::ServicedStray {
                        leg: st.role,
                        method: "INVITE".to_string(),
                        action: "auto-answered 200",
                    },
                    now,
                );
                st.obs.record(Observation::LegEarly { leg: st.role }, now);
                answer_initial_invite(st, uas, None).await?;
            }
        }
    }
    Ok(())
}

/// A provisional on the caller's establishing INVITE: anchor/feed the first
/// over-100, and PRACK a reliable one exactly once per `(fork tag, RSeq)` — a
/// retransmitted 183 is not double-PRACKed, while a FORKED 183 (distinct
/// To-tag, RFC 3261 §12.1.2) gets its OWN PRACK on its own early dialog. The
/// PRACK opens an "awaiting 200" ledger obligation the settle barrier holds on.
async fn absorb_establishing_provisional(
    st: &mut ActorState<'_>,
    inv: &mut ClientInvite,
    resp: &SipResponse,
    status: u16,
    now: Instant,
) -> Result<(), StepError> {
    // A 100 Trying is transaction plumbing, not an early dialog.
    if status <= 100 {
        return Ok(());
    }
    st.obs.record(Observation::LegEarly { leg: st.role }, now);
    if !st.saw_provisional {
        st.saw_provisional = true;
        st.ctx.anchor(&st.agent, "firstProvisional", resp);
        // The abandon body's `time_to_180` (default NONE on every other body).
        st.feed.on_provisional.stamp(st.ctx);
        if st.feed.ringing_gate {
            st.ctx.mark_ringing(true);
        }
    }
    if let Some(rseq) = reliable_rseq(resp) {
        let fork = resp.to.tag.clone().unwrap_or_default();
        if st.pracked_rseqs.insert((fork, rseq)) {
            let (_txn, req) = inv.try_prack_with_request(resp).await?;
            st.obs.record(
                Observation::RequestSent {
                    key: ObligationKey::new(st.role, ObligationKind::Prack, req.cseq.seq),
                    detail: "prack awaiting 200".to_string(),
                },
                now,
            );
            // C5: the reliable early dialog is now established AND PRACKed —
            // the observed fact an early UPDATE (RFC 3311 §5.1) gates on (a
            // real post-183 signal, unlike `LegPhase::Early` which holds
            // pre-183).
            st.obs.record(
                Observation::Subflow {
                    leg: st.role,
                    name: SUBFLOW_EARLY,
                    to: SubflowState::Answered,
                },
                now,
            );
        }
    }
    Ok(())
}

/// A non-2xx final on the caller's establishing INVITE. Returns `true` when a
/// §22.2 authenticated resend consumed the challenge (caller re-parks the
/// INVITE); otherwise records the terminal and — unless the next pending goal
/// is a RECEPTION goal, which then owns the verdict — surfaces the incidental
/// establishment failure as the linear `WrongStatus{expected: <180|183>}`,
/// never a 32 s barrier timeout.
async fn absorb_establishing_failure(
    st: &mut ActorState<'_>,
    inv: &mut ClientInvite,
    resp: &SipResponse,
    status: u16,
    now: Instant,
) -> Result<bool, StepError> {
    // RFC 3261 §22.2 authenticated retry: `absorb_response` already ACKed the
    // challenge (§17.1.1.3), so this goes straight to asking the responder for
    // a credential and resending ONCE (bumped CSeq, fresh branch). A second
    // challenge has `auth_retries_left == 0` and classifies as a plain
    // `status_401/407` deviation — never an unbounded loop.
    if matches!(status, 401 | 407) && st.auth_retries_left > 0 {
        if let Some(responder) = st.challenge_responder.clone() {
            if inv.ack_and_resend_with_auth(resp, responder.as_ref()).await? {
                st.auth_retries_left -= 1;
                // Re-point the scope's early CANCEL handle at the retried
                // transaction (its branch/CSeq changed).
                st.scope.set_early(inv.cancel_handle());
                return Ok(true);
            }
            // Responder DECLINED — surface the challenge as a plain deviation
            // (status_401/407), exactly as with no responder.
        }
    }
    st.obs.record(
        Observation::LegFinal { leg: st.role, status, reason: resp.reason.clone() },
        now,
    );
    st.obs.record(Observation::LegTerminated { leg: st.role }, now);
    st.scope.mark_terminated();
    if st.goals.has_pending() && !st.goals.next_step().is_some_and(GoalStep::is_reception) {
        return Err(StepError::WrongStatus {
            who: st.role.to_string(),
            expected: st.expected_provisional,
            got: status,
            reason: resp.reason.clone(),
        });
    }
    Ok(false)
}

/// Fold one inbound response into this leg's ordered response-fact log. The
/// typed message is retained only while a matcher-carrying reception goal is
/// still pending on this actor (the content matcher compares it later).
fn record_response_fact(st: &mut ActorState<'_>, resp: &SipResponse, now: Instant) {
    let retain = st
        .goals
        .remaining_steps()
        .any(|s| matches!(s, GoalStep::ExpectResponse { matcher: Some(_), .. }));
    let body_is_sdp = !resp.body.is_empty()
        && sip_message::message_helpers::get_header(&resp.headers, "content-type")
            .is_some_and(|v| v.to_ascii_lowercase().contains("sdp"));
    st.obs.record(
        Observation::LegResponse {
            leg: st.role,
            fact: ResponseFact {
                status: resp.status,
                reason: resp.reason.clone(),
                body_len: resp.body.len(),
                body_is_sdp,
                early_tag: resp.to.tag.clone(),
                typed: retain.then(|| Box::new(resp.clone())),
            },
        },
        now,
    );
}

async fn react_response(st: &mut ActorState<'_>, resp: SipResponse) -> Result<(), StepError> {
    let now = Instant::now();
    record_response_fact(st, &resp, now);
    // A response to our still-pending caller INVITE drives the establish flow —
    // but ONLY a response whose CSeq method is INVITE. A PRACK's 200 (or any
    // other in-dialog final) sharing the early dialog must NOT be fed to the
    // INVITE transaction (`absorb_response` would misread a PRACK 200 as the
    // INVITE being answered); it falls through to the obligation-closing path.
    if resp.cseq.method == "INVITE" {
        if let Some(mut inv) = st.dialogs.pending_invite.take() {
        match inv.absorb_response(&resp).await? {
            InviteResponseFate::Provisional { status } => {
                absorb_establishing_provisional(st, &mut inv, &resp, status, now).await?;
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
                // RETAIN the establishing INVITE (C1/E3): a LOSING fork's late
                // 2xx (§13.2.2.4) is ACK+BYE'd on a fork dialog derived from it.
                st.dialogs.won_invite = Some(inv);
            }
            InviteResponseFate::Failed { status } => {
                if absorb_establishing_failure(st, &mut inv, &resp, status, now).await? {
                    // §22.2 authenticated resend — the retried INVITE is a
                    // fresh pending transaction, parked back.
                    st.dialogs.pending_invite = Some(inv);
                }
            }
        }
        return Ok(());
        }
        // A NON-2xx final to a re-INVITE WE originated (C4/S5 glare): a `491
        // Request Pending` (§14.1) the peer sent because it had its OWN re-INVITE
        // outstanding when ours arrived. Hop-ACK it (§17.1.1.3 — `recv_any` does
        // not), CLOSE its ReInvite obligation (so a 491'd re-INVITE leaves no
        // open obligation), and schedule a RETRY after the §14.1 owner/non-owner
        // dwell (the dialog owner — the caller — backs off longer, so the two
        // retries no longer collide).
        if resp.cseq.method == "INVITE"
            && (300..700).contains(&resp.status)
            && st.sent_reinvites.contains(&resp.cseq.seq)
        {
            if let Some(txn) = st.sent_reinvite_txns.remove(&resp.cseq.seq) {
                txn.ack_non_2xx(&resp).await?;
            }
            st.sent_reinvites.remove(&resp.cseq.seq);
            st.obs.record(
                Observation::ResponseObserved {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, resp.cseq.seq),
                },
                now,
            );
            if resp.status == 491 {
                // §14.1: the owner of the Call-ID (the dialog's original UAC —
                // the ORIGINATING actor, keyed on its first goal) waits a random
                // T in [2.1, 4] s; a non-owner in [0, 2] s. Fixed in-range
                // values keep the paused-clock test deterministic while
                // preserving the owner>non-owner ordering that breaks the glare.
                let dwell = if st.originates {
                    Duration::from_millis(2500)
                } else {
                    Duration::from_millis(1000)
                };
                st.reinvite_retry = Some(Instant::now() + dwell);
            }
            return Ok(());
        }
        // A LATE 2xx from a LOSING fork (C1/E3, RFC 3261 §13.2.2.4): it echoes
        // the ESTABLISHING INVITE's CSeq but carries a DIFFERENT To-tag than the
        // confirmed (winner) dialog — a separate dialog this caller never chose.
        // ACK it on ITS OWN fork dialog (the ACK carries the fork's tag) then
        // terminate that fork with an immediate BYE. The BYE opens a `ForkBye`
        // obligation (closed by its tag-mismatched 200 below — NEVER terminating
        // this leg; the winning dialog lives on). Checked BEFORE the re-INVITE
        // 2xx path: a re-INVITE's 2xx always carries the confirmed tag.
        if (200..300).contains(&resp.status) {
            let is_losing_fork = st
                .dialogs
                .won_invite
                .as_ref()
                .is_some_and(|inv| inv.invite_cseq() == resp.cseq.seq)
                && st
                    .dialogs
                    .confirmed
                    .as_ref()
                    .zip(resp.to.tag.as_ref())
                    .is_some_and(|(d, t)| d.remote_tag() != t.as_str());
            if is_losing_fork {
                if let Some(inv) = st.dialogs.won_invite.as_ref() {
                    let mut fork = inv.fork_dialog(&resp);
                    // Our INVITE carried the offer, so the fork's 200 carried
                    // its answer — the ACK is bodyless (§13.2.2.4).
                    fork.ack_for(resp.cseq.seq, None).await;
                    let _bye =
                        fork.send_request(InDialogMethod::Bye).try_send().await?;
                    st.obs.record(
                        Observation::RequestSent {
                            key: ObligationKey::new(
                                st.role,
                                ObligationKind::ForkBye,
                                fork.local_cseq(),
                            ),
                            detail: "losing-fork hangup awaiting 200".to_string(),
                        },
                        now,
                    );
                }
                return Ok(());
            }
        }
        // A 2xx to an in-dialog INVITE with NO pending initial INVITE: this
        // caller's own delayed-offer re-INVITE (the `reinvite` body). ACK it WITH
        // the answer SDP (RFC 3264 §4 delayed offer) — IDEMPOTENTLY, re-derived
        // from the confirmed dialog + `resp.cseq`, NEVER gated on a one-shot a
        // lost-datagram interleaving could strand (that stranding is the bug this
        // fixes — mirrors the mux's `(Call-ID, CSeq)` re-ACK, the P0 contract).
        // Every such 2xx the reactor is handed is ACKed; closing the `ReInvite`
        // obligation, advancing the `reneg` teardown barrier, and stamping the
        // feed happen ONCE, keyed on the CSeq of a re-INVITE THIS leg originated.
        if (200..300).contains(&resp.status) && st.dialogs.confirmed.is_some() {
            let default = st.answer_body();
            let sdp = resolve_ack_body(
                &mut st.reinvite_ack_bodies,
                st.goals.next_step(),
                default,
                resp.cseq.seq,
            );
            if let Some(dialog) = st.dialogs.confirmed.as_mut() {
                dialog.ack_for(resp.cseq.seq, Some(&sdp)).await;
            }
            if st.sent_reinvites.remove(&resp.cseq.seq) {
                st.sent_reinvite_txns.remove(&resp.cseq.seq);
                let key = ObligationKey::new(st.role, ObligationKind::ReInvite, resp.cseq.seq);
                st.obs.record(Observation::ResponseObserved { key }, now);
                st.obs.record(
                    Observation::Subflow { leg: st.role, name: SUBFLOW_RENEG, to: SubflowState::Confirmed },
                    now,
                );
                // Count this completed cycle so an N-cycle re-INVITE script's
                // per-cycle barrier (reneg_count >= i) releases the next one —
                // serializing the chain (C6). Keyed on CSeq: a re-emitted 2xx
                // (a retransmit under loss) cannot double-count, and the
                // sent_reinvites guard already fires this block once per CSeq.
                st.obs.record(Observation::RenegCompleted { leg: st.role, cseq: resp.cseq.seq }, now);
                st.feed.on_reinvite_ok.stamp(st.ctx);
            }
            return Ok(());
        }
    }

    // A 491 to an UPDATE WE originated (C4/S6 collision): close its Update
    // obligation and RETRY after the back-off. UPDATE has NO ACK, so the 491
    // alone completes the transaction — nothing to hop-ACK. (The owner/non-owner
    // dwell mirrors §14.1 for a deterministic, glare-breaking retry order.)
    if resp.cseq.method == "UPDATE"
        && resp.status == 491
        && st.sent_updates.remove(&resp.cseq.seq)
    {
        st.obs.record(
            Observation::ResponseObserved {
                key: ObligationKey::new(st.role, ObligationKind::Update, resp.cseq.seq),
            },
            now,
        );
        let dwell = if st.originates {
            Duration::from_millis(2500)
        } else {
            Duration::from_millis(1000)
        };
        st.update_retry = Some(Instant::now() + dwell);
        return Ok(());
    }

    // Otherwise it is a final to one of our sent in-dialog requests (our BYE's
    // 200, our REFER's 202, our NOTIFY's 200, our PRACK's 200, …) — close the
    // obligation it opened and stamp the declared feed for the flow-advancing ones.
    if let Some(kind) = ObligationKind::from_cseq_method(resp.cseq.method.as_str()) {
        // The 200 to a LOSING-FORK BYE (C1/E3): same CSeq method (and possibly
        // the same CSeq number — fork spaces are independent, §12.2.1.1) as the
        // main BYE, but its To-tag echoes the LOSING fork's, not the confirmed
        // (winner) dialog's. It closes the `ForkBye` obligation WITHOUT
        // terminating this leg — the winning dialog lives on.
        let fork_teardown = kind == ObligationKind::Bye
            && st
                .dialogs
                .confirmed
                .as_ref()
                .zip(resp.to.tag.as_ref())
                .is_some_and(|(d, t)| d.remote_tag() != t.as_str());
        let kind = if fork_teardown { ObligationKind::ForkBye } else { kind };
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
                    st.sent_updates.remove(&resp.cseq.seq);
                    st.obs.record(
                        Observation::Subflow {
                            leg: st.role,
                            name: SUBFLOW_RENEG,
                            to: SubflowState::Confirmed,
                        },
                        now,
                    );
                    // Count the completed renegotiation uniformly with a
                    // re-INVITE (C6/S6), so a glare barrier can gate on
                    // `reneg_count` regardless of the offer's method.
                    st.obs.record(
                        Observation::RenegCompleted { leg: st.role, cseq: resp.cseq.seq },
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

/// The body of the ACK to an in-dialog INVITE 2xx: the pending
/// `ExpectResponse`'s `ack_body` override, else `default` (the engine-built
/// answer SDP) — resolved ONCE per CSeq and cached, so the ACK to a
/// re-surfaced 2xx is byte-identical (RFC 3261 §13.2.2.4) even after the goal
/// cursor advanced past the override-carrying goal.
pub(super) fn resolve_ack_body(
    cache: &mut HashMap<u32, String>,
    next_step: Option<&GoalStep>,
    default: &str,
    cseq: u32,
) -> String {
    if let Some(cached) = cache.get(&cseq) {
        return cached.clone();
    }
    let resolved = match next_step {
        Some(GoalStep::ExpectResponse { ack_body: Some(b), .. }) => {
            String::from_utf8_lossy(b).into_owned()
        }
        _ => default.to_string(),
    };
    cache.insert(cseq, resolved.clone());
    resolved
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
            originate_initial_invite(st, callee, plan, None).await?;
        }
        // The template twin of `Invite`: frozen headers/body ride verbatim,
        // routing and bookkeeping identical.
        GoalStep::InviteTemplate { callee, plan, template, opts } => {
            if template.method().map(|m| m.as_str()) != Some("INVITE") {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "InviteTemplate requires an INVITE request template".to_string(),
                });
            }
            originate_initial_invite(st, callee, plan, Some((template, opts))).await?;
        }
        // An in-dialog (or early-dialog) request from a template; method read
        // from the template. Opens the method's ledger obligation.
        GoalStep::RequestTemplate { template, opts, early } => {
            send_request_template(st, &template, opts, early).await?;
        }
        // Answer the bound server transaction from the template (status/reason
        // read from it) — provisional-non-consuming, final-consuming.
        GoalStep::RespondTemplate { template, opts, early } => {
            let Some((status, _)) = template.status() else {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "RespondTemplate requires a response template".to_string(),
                });
            };
            drive_respond(st, status, Some((&template, opts)), early, "RespondTemplate").await?;
        }
        // Answer the bound server transaction by POLICY — the completion verb.
        GoalStep::Respond { status } => {
            drive_respond(st, status, None, None, "Respond").await?;
        }
        GoalStep::ExpectResponse { status, body, early, ack_body: _, matcher } => {
            expect_response(st, status, body, early, matcher.as_ref())?;
        }
        GoalStep::ExpectRequest { kind, body, matcher } => {
            expect_request(st, &kind, body, matcher.as_ref())?;
        }
        GoalStep::ObserveFinal { key, expected } => {
            let fact = consume_final_fact(st)?;
            st.obs.record(
                Observation::ReplayFinal { key, expected, observed: fact.status },
                Instant::now(),
            );
        }
        GoalStep::ExpectFinal { assert } => {
            let fact = consume_final_fact(st)?;
            let (ok, want) = match assert {
                FinalAssert::Exact(s) => (fact.status == s, s),
                FinalAssert::Class(c) => (fact.status / 100 == c, c * 100),
                FinalAssert::NonError => (fact.status < 400, 200),
            };
            if !ok {
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: want,
                    got: fact.status,
                    reason: fact.reason,
                });
            }
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
            originate_reinvite(st).await?;
        }
        // An in-dialog UPDATE (RFC 3311) carrying this leg's offer: send it, open
        // the Update obligation; its 200 closes it (no ACK) and stamps
        // `on_update_ok` (see `react_response`).
        GoalStep::Update => {
            originate_update(st).await?;
        }
        // C5 (RFC 3311 §5.1): an EARLY UPDATE on the still-pending INVITE's
        // EARLY dialog (its reliable provisional already PRACKed). Sent through
        // the pending `ClientInvite` (which learned the early To-tag from the
        // 183), so it addresses the early dialog and rides its CSeq. Opens an
        // `Update` obligation + marks the offer outstanding (`sent_updates`);
        // its 200 closes it and releases the callee's held INVITE 200.
        GoalStep::UpdateEarly => {
            let now = Instant::now();
            let offer = st.media.offer_sdp().unwrap_or(crate::OFFER_SDP);
            let (key, req) = {
                let inv = st.dialogs.pending_invite.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "UpdateEarly with no pending early dialog".to_string(),
                    }
                })?;
                // Address the early dialog's learned To-tag so the UPDATE rides
                // that early dialog's OWN CSeq sequence (the same the PRACK used,
                // §12.2.1.1) — else it reuses the shared counter's value and
                // collides with the PRACK's CSeq on a SUT-less peer.
                let tag = inv.early_remote_tag().to_string();
                let mut req_builder = inv.send_request(InDialogMethod::Update).with_sdp(offer);
                if !tag.is_empty() {
                    req_builder = req_builder.with_to_tag(&tag);
                }
                let (_txn, req) = req_builder.try_send_with_request().await?;
                (ObligationKey::new(st.role, ObligationKind::Update, req.cseq.seq), req)
            };
            st.sent_updates.insert(req.cseq.seq);
            st.obs.record(
                Observation::RequestSent { key, detail: "early update awaiting 200".to_string() },
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
        // A plain in-dialog request (INFO/MESSAGE) carrying an optional typed
        // body + extra headers: send it on the confirmed dialog and open the
        // InDialog obligation keyed on its CSeq. Its 2xx closes it (no ACK, no
        // sub-flow — the `_` arm of `react_response`'s obligation match); a
        // dropped request or its 2xx holds the settle barrier until re-emitted,
        // exactly like a lost NOTIFY.
        GoalStep::InDialog { method, content_type, body, headers } => {
            originate_in_dialog(st, method, content_type, body, headers).await?;
        }
        GoalStep::Bye => {
            let now = Instant::now();
            // This leg is hanging up: discharge any in-dialog ack it still awaits
            // (a re-INVITE/realign it answered, a PRACK/UPDATE 200) BEFORE opening
            // the BYE's own obligation — the terminating dialog subsumes them
            // (§15), and the fresh BYE obligation below is still held to the 200.
            discharge_on_teardown(st, now);
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
        // Branch-conditional teardown (C2/E5): BYE the confirmed dialog if one
        // exists (the 200-wins branch), else a NO-OP (the CANCEL-wins branch —
        // the leg was 487'd, there is nothing to tear down). Same obligation
        // bookkeeping as `Bye` when a dialog exists.
        GoalStep::ByeIfConfirmed => {
            if st.dialogs.confirmed.is_none() {
                return Ok(());
            }
            let now = Instant::now();
            discharge_on_teardown(st, now);
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().expect("checked Some above");
                let _bye = dialog.bye().await;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone);
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
        GoalStep::ByeWith { headers } => {
            let now = Instant::now();
            discharge_on_teardown(st, now); // see GoalStep::Bye — subsume pending in-dialog acks
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
                    StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "ByeWith goal with no confirmed dialog".to_string(),
                    }
                })?;
                // Send the BYE carrying the extra headers (the deliberate
                // deviation); the reactor observes its 200 and closes the
                // obligation (we do not block on the final here).
                let mut req = dialog.send_request(InDialogMethod::Bye);
                for (name, value) in &headers {
                    req = req.with_header(name, value);
                }
                let _bye = req.try_send().await?;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
    }
    Ok(())
}

/// Originate a plain in-dialog request (INFO/MESSAGE) carrying an optional
/// typed body + extra headers on the confirmed dialog, opening the method's
/// ledger obligation — its 2xx alone closes it (the reactor observes it).
async fn originate_in_dialog(
    st: &mut ActorState<'_>,
    method: InDialogMethod,
    content_type: Option<String>,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) -> Result<(), StepError> {
    let now = Instant::now();
    let (key, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("{} goal with no confirmed dialog", method.as_str()),
            }
        })?;
        let mut req = dialog.send_request(method);
        match (body, content_type) {
            // A typed body rides `with_body` (Content-Type + Content-Length);
            // an untyped body still ships under a generic type so
            // Content-Length is emitted.
            (Some(bytes), ct) => {
                req = req.with_body(ct.as_deref().unwrap_or("application/octet-stream"), bytes);
            }
            // A content-type with no body: emit it as a header (`with_body`
            // only stamps Content-Type for a non-empty body).
            (None, Some(ct)) => req = req.with_header("Content-Type", &ct),
            (None, None) => {}
        }
        for (name, value) in &headers {
            req = req.with_header(name, value);
        }
        // The 2xx arrives through the reactor (recv_any); the returned
        // transaction handle is not awaited on here.
        let (_txn, request) = req.try_send_with_request().await?;
        // INFO/MESSAGE map to InDialog; any other method routed through this
        // goal opens under its own kind so its final still matches.
        let kind =
            ObligationKind::from_cseq_method(method.as_str()).unwrap_or(ObligationKind::InDialog);
        (ObligationKey::new(st.role, kind, request.cseq.seq), dialog.clone())
    };
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs.record(
        Observation::RequestSent { key, detail: format!("{} awaiting 2xx", method.as_str()) },
        now,
    );
    Ok(())
}

/// Originate the initial INVITE (plain or template-driven) and register the
/// caller bookkeeping — the shared realization of `Invite`/`InviteTemplate`.
async fn originate_initial_invite(
    st: &mut ActorState<'_>,
    callee: &'static str,
    plan: Option<crate::realcall::InvitePlan>,
    template: Option<(MessageTemplate, EmitOpts)>,
) -> Result<(), StepError> {
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
    if let Some((tmpl, opts)) = &template {
        builder = builder.template(tmpl, *opts);
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
    // A caller advertising `Supported: 100rel` (on the plan or a frozen
    // template header) awaits a reliable `183` — the `expected` of an
    // incidental shed/reject WrongStatus (linear `establish_100rel` parity).
    let advertises_100rel = plan.as_ref().is_some_and(|p| {
        p.headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case("supported") && v.to_ascii_lowercase().contains("100rel")
        })
    }) || template.as_ref().is_some_and(|(t, _)| {
        t.headers().iter().any(|h| {
            sip_message::message_helpers::name_matches("Supported", &h.name)
                && h.value.to_ascii_lowercase().contains("100rel")
        })
    });
    if advertises_100rel {
        st.expected_provisional = 183;
    }
    let call = builder.send().await;
    st.scope.set_early(call.cancel_handle());
    st.dialogs.pending_invite = Some(call);
    // The caller APPEARS the moment she originates — so `all_terminated`
    // cannot fire (and the runner exit) before she has processed her own
    // INVITE's final. Without this, a callee that terminates immediately
    // (the `invite_reject` 486) can make the obs "all terminated" while
    // the caller's leg has not yet recorded a fact, so the runner exits
    // before she ACKs the reject (RFC 3261 §17.1.1.3). Monotone: a later
    // provisional/answer only advances the phase.
    st.obs.record(Observation::LegEarly { leg: st.role }, Instant::now());
    Ok(())
}

/// Send a templated request on the confirmed dialog (or, `early`, on the
/// still-pending INVITE's early dialog — RFC 3311 §5.1) and open the method's
/// ledger obligation, mirroring the semantic goal's bookkeeping (re-INVITE →
/// outstanding offer + retained txn for the 491 hop-ACK; UPDATE → outstanding
/// offer; BYE → teardown discharge first).
async fn send_request_template(
    st: &mut ActorState<'_>,
    template: &MessageTemplate,
    opts: EmitOpts,
    early: bool,
) -> Result<(), StepError> {
    let now = Instant::now();
    let method = template
        .method()
        .and_then(|m| InDialogMethod::try_from(m).ok())
        .ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "RequestTemplate requires an in-dialog request template".to_string(),
        })?;
    if method == InDialogMethod::Bye && !early {
        // A hangup subsumes this leg's pending in-dialog acks (§15) exactly
        // like the semantic `Bye` goal.
        discharge_on_teardown(st, now);
    }
    let (txn, req, dialog_clone) = if early {
        let inv = st.dialogs.pending_invite.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "RequestTemplate{early} with no pending early dialog".to_string(),
            }
        })?;
        let tag = inv.early_remote_tag().to_string();
        let mut b = inv.send_request(method).template(template, opts);
        if !tag.is_empty() {
            b = b.with_to_tag(&tag);
        }
        let (txn, req) = b.try_send_with_request().await?;
        (txn, req, None)
    } else {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "RequestTemplate with no confirmed dialog".to_string(),
            }
        })?;
        let (txn, req) =
            dialog.send_request(method).template(template, opts).try_send_with_request().await?;
        (txn, req, Some(dialog.clone()))
    };
    let cseq = req.cseq.seq;
    let kind = ObligationKind::from_cseq_method(method.as_str()).unwrap_or(ObligationKind::InDialog);
    match method {
        InDialogMethod::Invite => {
            st.sent_reinvites.insert(cseq);
            st.sent_reinvite_txns.insert(cseq, txn);
        }
        InDialogMethod::Update => {
            st.sent_updates.insert(cseq);
        }
        _ => {}
    }
    if let Some(d) = dialog_clone {
        st.scope.set_confirmed(d); // refresh so a teardown BYE stays valid
    }
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, kind, cseq),
            detail: format!("templated {} awaiting final", method.as_str()),
        },
        now,
    );
    Ok(())
}

/// The stock reason phrase for a policy provisional.
fn provisional_reason(status: u16) -> &'static str {
    match status {
        180 => "Ringing",
        183 => "Session Progress",
        _ => "Progress",
    }
}

/// Answer the BOUND server transaction — the shared realization of
/// `RespondTemplate` (template payload) and `Respond` (policy payload). The
/// binding is the nearest preceding `ExpectRequest`'s consumed transaction,
/// else the parked initial INVITE. A status < 200 responds WITHOUT consuming
/// the binding; >= 200 consumes it with the disposition-equivalent bookkeeping
/// (dialog confirm / reject hop-ACK / teardown), keyed on the bound request.
async fn drive_respond(
    st: &mut ActorState<'_>,
    status: u16,
    template: Option<(&MessageTemplate, EmitOpts)>,
    early: Option<EarlyId>,
    step_name: &'static str,
) -> Result<(), StepError> {
    let now = Instant::now();
    let use_bound = st.bound.is_some();
    let parked_initial = st.parked.iter().position(|p| p.initial);
    if !use_bound && parked_initial.is_none() {
        // Fail-fast, bounded: the target was consumed by an automatic
        // (CANCEL → 487) or never existed — never a goal timeout.
        let detail = match st.parked_initial_consumed {
            Some(consumed) => format!(
                "{step_name}: the bound initial INVITE was consumed by an automatic ({consumed})"
            ),
            None => format!("{step_name}: no bound or parked transaction to answer"),
        };
        return Err(StepError::UnexpectedKind { who: st.role.to_string(), detail });
    }

    if status < 200 {
        // Provisional: respond in place, binding NOT consumed
        // (provisional-then-final on ONE server transaction, §17.2.1).
        let txn = match st.bound.as_mut() {
            Some(t) => t,
            None => &mut st.parked[parked_initial.expect("checked above")].txn,
        };
        let mut r = match template {
            Some((tmpl, opts)) => txn.respond_template(tmpl, opts),
            None => txn.respond(status, provisional_reason(status)),
        };
        if let Some(id) = early {
            // The fork id IS the fork's To-tag (RFC 3261 §12.1.2) — distinct
            // early dialogs on the one transaction.
            r = r.with_to_tag(id);
        }
        r.try_send().await?;
        st.obs.record(Observation::LegEarly { leg: st.role }, now);
        return Ok(());
    }

    // Final: consume the binding.
    let mut txn = match st.bound.take() {
        Some(t) => t,
        None => st.parked.remove(parked_initial.expect("checked above")).txn,
    };
    if let Some(id) = early {
        // The final's fork id names the WINNER: its tag becomes the sticky
        // dialog tag; the losing forks simply never receive a final (the
        // existing forked-UAS surface settles them).
        txn.adopt_to_tag(id);
    }
    let req_method = txn.request().method.as_str().to_string();
    let is_initial = req_method == "INVITE" && txn.request().to.tag.is_none();
    let cseq = txn.request().cseq.seq;

    {
        // `respond_template` derives status from the template; an early winner
        // tag was adopted above, so no per-response tag is needed here.
        let r = match template {
            Some((tmpl, opts)) => txn.respond_template(tmpl, opts),
            None if (200..300).contains(&status)
                && (req_method == "INVITE" || req_method == "UPDATE") =>
            {
                // A policy 2xx to an offer is never bodyless (RFC 3264 §5).
                txn.respond(status, "OK").with_sdp(st.media.answer_sdp().unwrap_or(crate::ANSWER_SDP))
            }
            None if (200..300).contains(&status) => txn.respond(status, "OK"),
            None => txn.respond(status, reject_reason(status)),
        };
        r.try_send().await?;
    }

    if (200..300).contains(&status) {
        if is_initial {
            note_uas_answered(st, &txn);
            st.feed.on_answer_sent.stamp(st.ctx);
        } else if req_method == "INVITE" {
            // A scripted 200 to a re-INVITE — same realign bookkeeping as the
            // reactive answer (the ACK confirms the sub-flow).
            st.answered_reinvites.insert(cseq);
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
        } else if req_method == "BYE" {
            // A scripted 200 to a BYE tears this leg down (§15).
            discharge_on_teardown(st, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
    } else if req_method == "INVITE" {
        // A non-2xx INVITE final awaits its hop-ACK (§17.2.1).
        arm_reject_final(st, &txn, status);
        if is_initial {
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
    }
    Ok(())
}

/// Consume this actor's next response facts up to (and including) the first
/// FINAL on its leg — provisionals before it are passed over. The goal-arm
/// gate guarantees one exists when a final-consuming goal fires.
fn consume_final_fact(st: &mut ActorState<'_>) -> Result<ResponseFact, StepError> {
    let facts: Vec<ResponseFact> =
        st.obs.with_snapshot(|s| s.leg(st.role).responses()[st.resp_seen..].to_vec());
    for (i, f) in facts.iter().enumerate() {
        if f.status >= 200 {
            st.resp_seen += i + 1;
            return Ok(f.clone());
        }
    }
    Err(StepError::UnexpectedKind {
        who: st.role.to_string(),
        detail: "final-consuming goal fired with no final observed".to_string(),
    })
}

/// `ExpectResponse`: strict, fail-fast consumption of the next response fact.
fn expect_response(
    st: &mut ActorState<'_>,
    status: u16,
    body: BodyExpect,
    early: Option<EarlyId>,
    matcher: Option<&MessageTemplate>,
) -> Result<(), StepError> {
    let facts: Vec<ResponseFact> =
        st.obs.with_snapshot(|s| s.leg(st.role).responses()[st.resp_seen..].to_vec());
    let fact = if status < 200 {
        // The NEXT response (100 Trying is transaction plumbing, skipped) must
        // be a provisional of exactly this status — a final arriving first, or
        // a different provisional, fails fast.
        let mut found = None;
        for (i, f) in facts.iter().enumerate() {
            if f.status == 100 {
                continue;
            }
            if f.status != status {
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: status,
                    got: f.status,
                    reason: f.reason.clone(),
                });
            }
            st.resp_seen += i + 1;
            found = Some(f.clone());
            break;
        }
        found
    } else {
        // Provisionals before the expected final are passed over.
        let mut found = None;
        for (i, f) in facts.iter().enumerate() {
            if f.status < 200 {
                continue;
            }
            if f.status != status {
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: status,
                    got: f.status,
                    reason: f.reason.clone(),
                });
            }
            st.resp_seen += i + 1;
            found = Some(f.clone());
            break;
        }
        found
    };
    let Some(fact) = fact else {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "ExpectResponse fired with no consumable response observed".to_string(),
        });
    };
    if let Some(id) = early {
        if fact.early_tag.as_deref() != Some(id) {
            return Err(StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!(
                    "ExpectResponse: fork mismatch — expected early id {id:?}, got tag {:?}",
                    fact.early_tag
                ),
            });
        }
    }
    check_body_expect(st.role, body, fact.body_len, fact.body_is_sdp)?;
    if let Some(tmpl) = matcher {
        let Some(resp) = &fact.typed else {
            return Err(StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "ExpectResponse matcher: the typed response was not retained".to_string(),
            });
        };
        tmpl.match_inbound(&SipMessage::Response(resp.as_ref().clone()), &MatchOpts::default()).map_err(
            |m| StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("response did not match its template: {m}"),
            },
        )?;
    }
    Ok(())
}

/// `ExpectRequest`: consume the next parked request of this kind into the
/// actor's bound transaction; the matcher runs at consume time on the parked
/// transaction's request.
fn expect_request(
    st: &mut ActorState<'_>,
    kind: &RequestKind,
    body: BodyExpect,
    matcher: Option<&MessageTemplate>,
) -> Result<(), StepError> {
    let Some(idx) = st.parked.iter().position(|p| parked_matches(p, kind)) else {
        let detail = match (kind, st.parked_initial_consumed) {
            (RequestKind::Initial, Some(consumed)) => format!(
                "ExpectRequest: the parked initial INVITE was consumed by an automatic ({consumed})"
            ),
            _ => "ExpectRequest fired with no matching parked request".to_string(),
        };
        return Err(StepError::UnexpectedKind { who: st.role.to_string(), detail });
    };
    let entry = st.parked.remove(idx);
    let req = entry.txn.request();
    let body_is_sdp = !req.body.is_empty()
        && sip_message::message_helpers::get_header(&req.headers, "content-type")
            .is_some_and(|v| v.to_ascii_lowercase().contains("sdp"));
    check_body_expect(st.role, body, req.body.len(), body_is_sdp)?;
    if let Some(tmpl) = matcher {
        entry.txn.expect_template(tmpl, &MatchOpts::default()).map_err(|m| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("request did not match its template: {m}"),
            }
        })?;
    }
    st.bound = Some(entry.txn);
    Ok(())
}

/// Enforce a reception goal's [`BodyExpect`], fail-fast with a bounded detail.
fn check_body_expect(
    role: &'static str,
    body: BodyExpect,
    body_len: usize,
    body_is_sdp: bool,
) -> Result<(), StepError> {
    let ok = match body {
        BodyExpect::Any => true,
        BodyExpect::Present => body_len > 0,
        BodyExpect::SdpPresent => body_is_sdp,
    };
    if ok {
        Ok(())
    } else {
        Err(StepError::UnexpectedKind {
            who: role.to_string(),
            detail: format!("body expectation {body:?} not met (len {body_len})"),
        })
    }
}

/// Originate ONE delayed-offer (bodyless) re-INVITE on the confirmed dialog and
/// register its bookkeeping: open the `ReInvite` obligation keyed on its CSeq,
/// track the CSeq in `sent_reinvites` (the 2xx-ACK / completion path) and retain
/// the client transaction in `sent_reinvite_txns` so a NON-2xx final (a 491
/// glare reject, C4/S5) can be hop-ACKed. Shared by [`GoalStep::Reinvite`] and
/// the §14.1 glare RETRY arm — so a retried re-INVITE is byte-identical to the
/// first (fresh CSeq, same delayed-offer shape).
async fn originate_reinvite(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let (key, dialog_clone, txn) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Reinvite with no confirmed dialog".to_string(),
        })?;
        let txn = dialog.request(InDialogMethod::Invite, None).await;
        let cseq = dialog.local_cseq();
        (ObligationKey::new(st.role, ObligationKind::ReInvite, cseq), dialog.clone(), txn)
    };
    st.sent_reinvites.insert(key.cseq);
    st.sent_reinvite_txns.insert(key.cseq, txn);
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs.record(
        Observation::RequestSent { key, detail: "re-INVITE awaiting 2xx".to_string() },
        now,
    );
    Ok(())
}

/// Park until the pending §14.1 glare retry is due (or forever if none).
async fn wait_reinvite_retry(retry: &Option<Instant>) {
    match retry {
        Some(at) => tokio::time::sleep_until(*at).await,
        None => std::future::pending().await,
    }
}

/// Originate ONE in-dialog UPDATE (RFC 3311) carrying this leg's offer, opening
/// the `Update` obligation and marking the offer OUTSTANDING (`sent_updates`).
/// Shared by [`GoalStep::Update`] and the S6 collision RETRY arm.
async fn originate_update(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let offer = st.media.offer_sdp().unwrap_or(crate::OFFER_SDP);
    let (key, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Update with no confirmed dialog".to_string(),
        })?;
        let _upd =
            dialog.send_request(InDialogMethod::Update).with_sdp(offer).try_send().await?;
        let cseq = dialog.local_cseq();
        (ObligationKey::new(st.role, ObligationKind::Update, cseq), dialog.clone())
    };
    st.sent_updates.insert(key.cseq);
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs
        .record(Observation::RequestSent { key, detail: "update awaiting 200".to_string() }, now);
    Ok(())
}

/// Park until the pending S6 UPDATE-collision retry is due (or forever if none).
async fn wait_update_retry(retry: &Option<Instant>) {
    match retry {
        Some(at) => tokio::time::sleep_until(*at).await,
        None => std::future::pending().await,
    }
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
