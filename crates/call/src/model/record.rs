//! The master [`Call`] record and its call-level satellites: lifecycle state,
//! active peering, the a-leg INVITE
//! snapshot, policy overrides, active rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::limiter::CallLimiterState;

use super::cdr::CdrEvent;
use super::decision_log::DecisionMark;
use super::leg::Leg;
use super::provisional::Provisional;
use super::services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, RerouteState, TransferState,
};
use super::sm::{MachineId, StateLabel};
use super::termination::Termination;
use super::timer::TimerEntry;

/// A single `name: value` header line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SipHeader {
    pub name: String,
    pub value: String,
}

/// Snapshot of the original a-leg INVITE — source of truth for failover b-leg
/// reconstruction and INVITE-response relay (§8.2.6.2). Written when the call
/// is built; only another owner's body, taken whole, replaces it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ALegInviteSnapshot {
    pub uri: String,
    pub headers: Vec<SipHeader>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
    /// The INVITE's CSeq number: what a copy of it carries (RFC 3261
    /// §8.2.2.2), and a new request on its Call-ID and From-tag does not.
    pub cseq: u32,
    /// The INVITE's From-tag: the caller's tag, the peer tag of every
    /// caller-facing dialog (RFC 3261 §12.1.1).
    pub from_tag: String,
}

/// A rule activated on this call by the HTTP API response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRule {
    /// Rule identifier — matches a registered RuleDefinition id.
    pub id: String,
    /// Whether currently active (can be deactivated mid-call).
    pub active: bool,
}

/// Call lifecycle: `active` → `terminating` (BYEs sent, awaiting resolution) →
/// `terminated` (removable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallModelState {
    Active,
    Terminating,
    Terminated,
}

/// The single active leg pair (1↔1). `None` on the [`Call`] means 1↔0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePeer {
    pub leg_a: String,
    pub leg_b: String,
}

/// Body override derived from active features. Wrapped in `Option` on the
/// [`Call`], preserving a three-way distinction: `None` = no override,
/// `Some(Empty)` = force empty body, `Some(Bytes)` = substitute this body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyUpdateBody {
    Empty,
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
}

/// Master call record. `callRef` is derived from the a-leg identifiers (see
/// [`crate::callref`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// Deterministic: derived from a-leg call-id + from-tag (+ primary ordinal).
    pub call_ref: String,
    pub a_leg: Leg,
    /// The legs this stack dialled, in creation order ("b-1", "b-2", …, in
    /// the [`leg_space`](Self::leg_space) that minted each): destination,
    /// transfer-target and media legs alike.
    pub b_legs: Vec<Leg>,
    /// The routing answers the call dialled, in opening order
    /// ([`crate::helpers::attempt`] is the writer).
    pub attempts: Vec<super::attempt::Attempt>,
    /// Active peering — the typed pair makes N↔N unrepresentable. `None` = 1↔0.
    pub active_peer: Option<ActivePeer>,
    pub callback_context: Option<String>,
    /// Opaque adapter-owned attribution blob (latest-wins; emitted into the CDR).
    /// Absent and null both mean "no attribution".
    pub billing_context: Option<String>,
    /// Snapshot of the original a-leg INVITE; never mutated.
    pub a_leg_invite: ALegInviteSnapshot,
    /// The call's admission state on the call limiter, under its own key.
    pub limiter: CallLimiterState,
    /// Serializable timer intents (not runtime fibers): at most one entry per
    /// key.
    pub timers: Vec<TimerEntry>,
    /// The last generation a timer was armed under: each arming takes the
    /// next, so a fire names the one instance it belongs to.
    pub timer_generation: u64,
    pub cdr_events: Vec<CdrEvent>,
    pub state: CallModelState,
    pub created_at: i64,
    pub trace_id: Option<String>,
    pub root_span_id: Option<String>,
    pub sampled: Option<bool>,
    pub worker_index: Option<i64>,
    /// True if this call carries an emergency Resource-Priority.
    pub emergency: Option<bool>,
    /// Feature activations decoded from the decision-engine response.
    pub features: Option<crate::features::FeatureActivations>,
    /// Header overrides derived from active features (`None` value = drop).
    pub policy_update_headers: Option<BTreeMap<String, Option<String>>>,
    /// Body override derived from active features (see [`PolicyUpdateBody`]).
    pub policy_update_body: Option<PolicyUpdateBody>,
    /// Rules activated on this call by the HTTP API response.
    pub active_rules: Option<Vec<ActiveRule>>,
    /// Per-service opaque extension slot (ADR-0016); key presence activates the
    /// owning service.
    pub ext: Option<ExtMap>,
    /// Per-call message counter for cap-defense (defaults to 0).
    pub message_count: Option<i64>,
    /// Leg IDs that already triggered one safety-timer refresh while terminating.
    pub terminating_refresh_legs: Option<Vec<String>>,
    /// Per-call runtime state for the `relayFirst18xTo180` service (typed slice;
    /// ADR-0016's full typed-ext is out of scope). `None` until the first 18x is
    /// processed under an active strategy.
    pub relay_first_18x: Option<RelayFirst18xState>,
    /// Per-call runtime state for the `promote18xPemTo200` service (strategy
    /// `promote-pem-to-200`; typed slice). `None` until the first 183+PEM is
    /// promoted.
    pub promote_pem: Option<PromotePemState>,
    /// Per-call runtime state for the REFER blind-transfer service (typed
    /// slice). `None` until a REFER is intercepted; presence is the
    /// service-activation guard. Cleared (`None`) on every terminal transition.
    pub transfer: Option<TransferState>,
    /// Internal release events the decision backend **subscribed to** on the
    /// last applied `Route`: when a subscribed event fires (max-call-duration
    /// first), the core consults the engine's `call_release` instead of tearing
    /// the call down locally; an unsubscribed event keeps the local
    /// `BeginTermination`. **Call-scoped, not per-leg**: the only v1 event
    /// (`GlobalDuration`) is call-scoped, so a per-leg registry would be
    /// speculative — revisit when a genuinely per-leg event (BYE/INFO
    /// reporting) lands. Replicated like `features` (an ordinary `Call` field
    /// on the msgpack body), so a restored record keeps honoring the
    /// subscription.
    pub subscriptions: Vec<ReleaseEventKind>,
    /// Per-call runtime state for an in-flight **established-call reroute**
    /// (a `Route`-shaped `call_release` decision): replacement b-leg dialing →
    /// a-leg re-INVITE realign → old-leg BYE. Mirrors `transfer` (typed slice;
    /// presence is the activation guard for the `release-reroute` rules).
    /// `None` when no reroute is in flight.
    pub reroute: Option<RerouteState>,
    /// Every reliable provisional the call received (RFC 3262), in arrival
    /// order, for the life of the call: the engine records each one before
    /// any rule reads it ([`crate::helpers::reliable`] is the one writer).
    /// Replicated with the call, so a restored record discards a responder's
    /// retransmission, translates a PRACK's `RAck` and answers a late PRACK
    /// as the primary would.
    pub provisionals: Vec<Provisional>,
    /// The `seq` of the last message recorded on any leg's ring — the
    /// call-wide sequence [`crate::helpers::record_message`] draws from; `0`
    /// while nothing is recorded.
    pub message_seq: u32,
    /// The decisions applied to the call, in order
    /// ([`crate::helpers::mark_decision`] is the one writer); empty until the
    /// first decision. On a [`setup_cancelled`](Self::setup_cancelled) call,
    /// the decisions that answered, none of them applied.
    pub decision_log: Vec<DecisionMark>,
    /// The count of applied decisions — `decision_log.len()`, kept as a field
    /// so a stamp reads it without a length; `0` before the first decision.
    pub decision_ordinal: u32,
    /// The caller CANCELed the setup while its initial decision was in
    /// flight: the caller holds the transaction layer's 487, and nothing the
    /// decision stated was applied.
    #[serde(default)]
    pub setup_cancelled: bool,
    /// Who ended the call and why ([`crate::helpers::record_termination`] is
    /// the one writer, the first termination's record stands); `None` while
    /// the call is live.
    pub termination: Option<Termination>,
    /// The turn being handled (one event under the call's lock), whose number
    /// every ring entry it appends carries; `1` for the initial INVITE's turn,
    /// advanced by one as each turn lands ([`crate::helpers::seal_turn`]).
    /// A restored record resumes from the last flushed body: later turns were
    /// never written, so no surviving entry shares a number.
    pub message_turn: u32,
    /// Per-call state-machine cursors (ADR-0016 X4): the single home for every
    /// active machine's current state label, keyed by [`MachineId`]. The
    /// `SetState` action is its sole writer; the rule engine reads it to gate
    /// machine-bound rules.
    pub sm_cursors: BTreeMap<MachineId, StateLabel>,
    /// The seed every tag this stack mints toward the caller is derived from,
    /// drawn once at the call's birth from the keyed identifier stream and
    /// replicated with the body: any owner of the call derives the tags its
    /// first owner showed the caller (RFC 3261 §19.3, ADR-0055 §4).
    #[serde(default)]
    pub tag_seed: u64,
    /// The leg-id space the node acting on the call mints from: the creating
    /// node's from the call's birth, set by whichever node acts on it.
    #[serde(default)]
    pub leg_space: super::leg_space::LegSpace,
}

impl Call {
    /// The call's **incarnation**: which of the successive calls a `call_ref`
    /// can carry this record is. A retried INVITE reuses its Call-ID and From
    /// tag, so it is born on the `call_ref` of the call it retries; the
    /// incarnation tells the two apart. It is the limiter key, minted once at
    /// creation and replicated with the body, so every copy of one call agrees.
    pub fn incarnation(&self) -> &str {
        self.limiter.key()
    }

    /// The incarnation's mark ([`crate::incarnation_mark`]): what tells this
    /// call apart from the other calls of its `call_ref`, stamped on every Via
    /// and Contact the stack emits for it.
    pub fn incarnation_mark(&self) -> &str {
        crate::incarnation_mark(self.incarnation())
    }
}
