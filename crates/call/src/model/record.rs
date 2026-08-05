//! The master [`Call`] record and its call-level satellites: lifecycle state,
//! HA topology hint, active peering, limiter entries, the a-leg INVITE
//! snapshot, tag mappings, policy overrides, active rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::cdr::CdrEvent;
use super::leg::Leg;
use super::services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, RerouteState, TransferState,
};
use super::sm::{MachineId, StateLabel};
use super::timer::TimerEntry;

/// Maps a B-leg's real tag to the B2BUA-generated tag shown to Alice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagMapping {
    /// B2BUA-generated tag shown to Alice.
    pub a_tag: String,
    /// Which B-leg this maps to.
    pub b_leg_id: String,
    /// Bob's actual remote tag.
    pub b_tag: String,
}

/// One reliable provisional relayed toward the caller (RFC 3262): the a-facing
/// `RSeq` this stack minted and the b-leg response it stands for. The caller
/// PRACKs the number she was shown, so the relayed `RAck` translates back
/// through this map before it reaches the callee that owns the other sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliableProvisional {
    /// The `RSeq` shown to the caller — this stack's own sequence.
    pub a_rseq: i64,
    /// The b-leg the provisional came from.
    pub b_leg_id: String,
    /// The `RSeq` that b-leg stated.
    pub b_rseq: i64,
}

/// Active limiter entry on a call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLimiterState {
    pub limiter_id: String,
    pub limit: i64,
    /// Rounded timestamp when this call's count was INCRed.
    pub origin_window: i64,
    /// Whether the matching INCR actually succeeded. `Some(false)` = fail-open
    /// admission → the termination DECR must be skipped. `None` on pre-fix
    /// entries (which all reflect successful INCRs).
    pub increment_succeeded: Option<bool>,
}

/// A single `name: value` header line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SipHeader {
    pub name: String,
    pub value: String,
}

/// Snapshot of the original a-leg INVITE — source of truth for failover b-leg
/// reconstruction and INVITE-response relay (§8.2.6.2). Never mutated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ALegInviteSnapshot {
    pub uri: String,
    pub headers: Vec<SipHeader>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
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

/// Persisted topology hint: the worker pair stamped at INVITE time plus a
/// per-context **version vector** `(gen, bak_gen)` = `(p, b)` (ADR-0014).
///
/// `gen` (`p`) is the **primary** counter — bumped only by the call's primary on
/// a local mutation. `bak_gen` (`b`) is the **backup** counter — bumped only by
/// an acting-backup on a takeover mutation. Each node bumps only its own role's
/// counter, so the *other* node's counter in an incoming update is, by
/// construction, the branch point. A single-counter "highest gen wins" LWW
/// cannot disambiguate concurrent primary+backup mutations. See the apply
/// rule in `b2bua::repl::puller`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTopology {
    pub pri: String,
    pub bak: String,
    /// Primary counter `p` of the `(p,b)` version vector.
    pub gen: i64,
    /// Backup counter `b` of the `(p,b)` version vector. `#[serde(default)]` so
    /// a body serialised without it still deserialises (`b = 0`).
    #[serde(default)]
    pub bak_gen: i64,
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
    /// Ordered by attempt ("b-1", "b-2", …).
    pub b_legs: Vec<Leg>,
    /// Active peering — the typed pair makes N↔N unrepresentable. `None` = 1↔0.
    pub active_peer: Option<ActivePeer>,
    pub callback_context: Option<String>,
    /// Opaque adapter-owned attribution blob (latest-wins; emitted into the CDR).
    /// Absent and null both mean "no attribution".
    pub billing_context: Option<String>,
    /// Snapshot of the original a-leg INVITE; never mutated.
    pub a_leg_invite: ALegInviteSnapshot,
    pub limiter_entries: Vec<CallLimiterState>,
    /// Serializable timer intents (not runtime fibers).
    pub timers: Vec<TimerEntry>,
    pub cdr_events: Vec<CdrEvent>,
    pub state: CallModelState,
    pub created_at: i64,
    /// Via headers from the most recent non-INVITE a-leg request (for relay).
    pub a_leg_pending_vias: Option<Vec<String>>,
    /// CSeq of the most recent non-INVITE a-leg request (echoed on response).
    pub a_leg_pending_cseq: Option<i64>,
    /// Maps B-leg remote To-tags to B2BUA-generated tags shown to Alice.
    pub tag_map: Vec<TagMapping>,
    pub trace_id: Option<String>,
    pub root_span_id: Option<String>,
    pub sampled: Option<bool>,
    pub worker_index: Option<i64>,
    /// HA topology hint.
    #[serde(rename = "_topology")]
    pub topology: Option<CallTopology>,
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
    /// on the msgpack body), so a takeover node keeps honoring the
    /// subscription. `#[serde(default)]` so a body encoded before this field
    /// decodes as "no subscriptions".
    #[serde(default)]
    pub subscriptions: Vec<ReleaseEventKind>,
    /// Per-call runtime state for an in-flight **established-call reroute**
    /// (a `Route`-shaped `call_release` decision): replacement b-leg dialing →
    /// a-leg re-INVITE realign → old-leg BYE. Mirrors `transfer` (typed slice;
    /// presence is the activation guard for the `release-reroute` rules).
    /// `None` when no reroute is in flight.
    #[serde(default)]
    pub reroute: Option<RerouteState>,
    /// The reliable provisionals relayed toward the caller, in mint order
    /// (RFC 3262 §7.1). The a-facing `RSeq` sequence belongs to the a-leg
    /// INVITE transaction, so it survives here: a PRACK arriving after a
    /// takeover still translates onto the b-leg number it acknowledges.
    #[serde(default)]
    pub reliable_provisionals: Vec<ReliableProvisional>,
    /// Per-call state-machine cursors (ADR-0016 X4): the single home for every
    /// active machine's current state label, keyed by [`MachineId`]. The
    /// `SetState` action is its sole writer; the rule engine reads it to gate
    /// machine-bound rules. `#[serde(default, skip_serializing_if)]` keeps
    /// old/new bodies interoperable under the positional msgpack codec — empty
    /// maps drop off the wire and absent maps decode to empty, so this MUST
    /// remain the last `Call` field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sm_cursors: BTreeMap<MachineId, StateLabel>,
}
