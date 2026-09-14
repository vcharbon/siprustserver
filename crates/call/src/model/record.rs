//! The master [`Call`] record and its call-level satellites: lifecycle state,
//! HA topology hint, active peering, limiter entries, the a-leg INVITE
//! snapshot, tag mappings, policy overrides, active rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::cdr::CdrEvent;
use super::decision_log::DecisionMark;
use super::emission::RetainedEmission;
use super::leg::Leg;
use super::services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, RerouteState, TransferState,
};
use super::sm::{MachineId, StateLabel};
use super::termination::Termination;
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
/// early dialog it was shown in, the `RSeq` this stack minted for it, and the
/// b-leg response it stands for. The caller PRACKs the number she was shown, so
/// the relayed `RAck` translates back through this map before it reaches the
/// callee that owns the other sequence.
///
/// `(b_leg_id, b_tag, b_cseq, b_rseq)` identifies the provisional. Every part
/// earns its place: RFC 3262 §3 (errata 4600) makes each callee fork's sequence
/// independent, so two forks of one leg may state the SAME `RSeq` and only the
/// fork tag tells them apart; and §3 restarts the sequence per INVITE
/// transaction, so without the `CSeq` a re-INVITE's first provisional could
/// collide with the initial INVITE's. Either collision would be misread as a
/// retransmission and answered with a number minted for another provisional.
/// `(a_tag, a_rseq)` identifies it from the caller's side, which is the side a
/// PRACK arrives from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliableProvisional {
    /// The dialog the provisional was SHOWN in — the To-tag of this stack's
    /// own on the face it relayed toward: the a-face on an initial INVITE or a
    /// caller re-INVITE, the b-face on a callee re-INVITE. Each such dialog
    /// carries its own sequence (RFC 3262 §4, errata 4603).
    pub a_tag: String,
    /// The `RSeq` shown there — this stack's own sequence.
    pub a_rseq: i64,
    /// The leg the provisional came from — the responder's.
    pub b_leg_id: String,
    /// The responder's own tag on that leg's dialog — a callee fork's early
    /// dialog, or the caller's confirmed one.
    pub b_tag: String,
    /// The `CSeq` number of the responder-facing INVITE transaction it answers.
    pub b_cseq: i64,
    /// The `RSeq` the responder stated.
    pub b_rseq: i64,
    /// Whether the matching PRACK has been received on the face the number
    /// was shown (RFC 3262 §3): a `true` entry is off the unacknowledged list
    /// — its retransmissions there have ceased, so a responder repeat of it
    /// is absorbed rather than relayed. The entry itself stays for the life of
    /// the call: it still translates a re-PRACK's `RAck` and anchors the
    /// ladder.
    #[serde(default)]
    pub acknowledged: bool,
    /// The provisional as it left on the face the number was shown, on the
    /// RFC 3262 §3 ladder this stack's retransmissions repeat
    /// ([`RetainedEmission`], paced). `Some` while the ladder is live; `None`
    /// before the provisional leaves, once the PRACK retires it, and once the
    /// ladder ceases — retained bytes are never sent again after any of those.
    #[serde(default)]
    pub emission: Option<RetainedEmission>,
    /// The `CSeq` number of the INVITE the provisional answers on the face it
    /// was shown — the second `RAck` token a PRACK names beside `a_rseq`
    /// (RFC 3262 §7.2).
    /// `None` on an entry hydrated from a peer that recorded none: absent
    /// books disprove no PRACK, so such an entry admits any CSeq token. The
    /// replication body is positional, so this stays the LAST field and the
    /// fields before it are never skipped: `#[serde(default)]` hydrates only a
    /// missing trailing element.
    #[serde(default)]
    pub a_cseq: Option<i64>,
}

/// A reliable provisional this stack acknowledged ITSELF, on the responder's
/// face: the originator never offered `100rel`, or a masking policy hid the
/// provisional from it, so the PRACK the responder is owed is this stack's own
/// and no shown number exists. Keyed the way the responder identifies the
/// provisional (RFC 3262 §3, §7.1): its leg, its tag on that leg's dialog, the
/// `CSeq` of the INVITE it answers and the `RSeq` it stated. One entry per
/// provisional, for the life of the call: a repeat of it is the responder's §3
/// retransmission — discarded where it arrives (§4), never PRACKed twice.
/// Replicated with the call, so a takeover node discards the repeat too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrackedProvisional {
    /// The responder's leg.
    pub leg_id: String,
    /// The responder's own tag on that leg's dialog.
    pub remote_tag: String,
    /// The `CSeq` number of the responder-facing INVITE transaction it answers.
    pub invite_cseq: i64,
    /// The `RSeq` the responder stated.
    pub rseq: i64,
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
    /// (RFC 3262 §7.1) — one entry per provisional this stack renumbered, for
    /// the life of the call. It survives here because a PRACK arriving after a
    /// takeover still translates onto the b-leg number it acknowledges.
    #[serde(default)]
    pub reliable_provisionals: Vec<ReliableProvisional>,
    /// The reliable provisionals this stack PRACKed on the responder's behalf
    /// (no shown number, so no `reliable_provisionals` entry), one per
    /// provisional for the life of the call — the books that make a
    /// responder's retransmission of one recognisable as such.
    #[serde(default)]
    pub pracked_provisionals: Vec<PrackedProvisional>,
    /// The `seq` of the last message recorded on any leg's ring — the
    /// call-wide sequence [`crate::helpers::record_message`] draws from; `0`
    /// while nothing is recorded.
    #[serde(default)]
    pub message_seq: u32,
    /// The decisions applied to the call, in order
    /// ([`crate::helpers::mark_decision`] is the one writer); empty until the
    /// first decision.
    #[serde(default)]
    pub decision_log: Vec<DecisionMark>,
    /// The count of applied decisions — `decision_log.len()`, kept as a field
    /// so a stamp reads it without a length; `0` before the first decision.
    #[serde(default)]
    pub decision_ordinal: u32,
    /// Who ended the call and why ([`crate::helpers::record_termination`] is
    /// the one writer, the first termination's record stands); `None` while
    /// the call is live.
    #[serde(default)]
    pub termination: Option<Termination>,
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
