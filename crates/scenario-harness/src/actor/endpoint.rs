//! The declarative per-endpoint vocabulary: how an endpoint answers its initial
//! INVITE ([`Disposition`]), the media it negotiates ([`MediaState`]), which
//! reactive events feed the load report ([`CtxFeed`]/[`Feed`]), the sub-flow
//! names, and the full endpoint declaration ([`ActorSpec`]). The LIVE state a
//! spec is wired into does NOT live here — see [`super::runner::ActorState`].
//!
//! # Downstream-contract feeding is DECLARATIVE ([`CtxFeed`])
//!
//! Phases / checkpoints / the 18x ringing gate key the load report's case
//! buckets and the chaos classifier's phase-transition proximity (see
//! `docs/todos/actor-harness-p1-contract-table.md`), and each call body stamps
//! a DIFFERENT trail (the refer body stamps only `referred`/`transferred` and
//! never feeds `mark_ringing`; basic stamps `connected`/`bye_200` and does).
//! So the reactor stamps NOTHING on its own — each [`ActorSpec`] declares
//! exactly which reactive event feeds which label, and an undeclared event
//! feeds nothing. Message ANCHORS are the exception: they are attached
//! generically at reaction time with the message in hand (they are inert
//! unless the shape publishes them and the call is sampled).

use std::net::SocketAddr;
use std::time::Duration;

use sip_message::{CseqPattern, DelayedAutomatic};

use super::goals::Goal;
use crate::realcall::CallCtx;
use crate::Agent;

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
/// the endpoint state machine's entry policy. Later in-dialog traffic is always
/// handled reactively by [`super::react::default_react`], regardless of
/// disposition.
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
    /// a NO-ANSWER-triggered failover needs: the INVITE server
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
    pub(super) fn answer_sdp(&self) -> Option<&'static str> {
        self.answer.or(self.offer)
    }

    /// The SDP to offer on an originated INVITE.
    pub(super) fn offer_sdp(&self) -> Option<&'static str> {
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

    pub(super) fn stamp(&self, ctx: &CallCtx) {
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

/// The declarative spec for one endpoint — what a scenario DECLARES; the runner
/// turns it into an [`super::runner::ActorState`] wired to the shared observed
/// state.
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
    /// A declared CSeq relative-pattern deviation (ADR-0024 §6). Attached at
    /// EVERY dialog-formation point of this actor with ONE shared step counter,
    /// so a scope-refresh clone never forks it. `None` = stack numbering.
    pub cseq: Option<CseqPattern>,
    /// A declared delayed automatic (ADR-0024 §6): hold this actor's originated
    /// INVITE's automatic ACK-to-2xx for a duration. `None` = fire immediately.
    pub delayed: Option<DelayedAutomatic>,
}
