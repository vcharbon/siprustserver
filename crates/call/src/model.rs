//! The B2BUA call data model — Rust port of `src/call/CallModel.ts`.
//!
//! Three-level hierarchy: [`Call`] → [`Leg`] → [`Dialog`]. The whole tree is a
//! pure, serializable value (no Effect, no I/O, no runtime). It round-trips
//! through the body codec ([`crate::codec`]) for persistence.
//!
//! ## Porting notes (see ADR-0008)
//!
//! - **Optional vs null.** Effect `Schema.optional` and `Schema.NullOr` both
//!   become `Option<T>` (msgpack collapses absent/nil). The one place the source
//!   carries a *three-way* absent/null/value distinction that is behaviourally
//!   load-bearing — `Call.policyUpdateBody` — is preserved as
//!   [`PolicyUpdateBody`] (`None` = no override, `Some(Empty)` = force empty
//!   body, `Some(Bytes)` = substitute). `billingContext` (`optional(NullOr)`) is
//!   not load-bearing on absence-vs-null and collapses to `Option<String>`.
//! - **Opaque `ext`.** Per-service slices are carried verbatim as
//!   [`ExtMap`] (`serde_json::Value`); core never interprets them.
//! - **Maps are `BTreeMap`** (not `HashMap`) so encode is deterministic
//!   (codec property P2).
//! - **Byte fields** (`aLegInvite.body`, `cachedSdp`, INVITE handle) use
//!   `serde_bytes` so msgpack stores them as `bin`, and the INVITE handle keeps
//!   the request as raw bytes — the data model takes no `sip-message` dep.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-service opaque extension carry, keyed by callflow-service id. Core never
/// interprets the values; each service decodes its own slice at the rule layer.
pub type ExtMap = BTreeMap<String, serde_json::Value>;

// ── State-machine identifiers (ADR-0016) ─────────────────────────────────────

/// Identifier of a per-call state machine (ADR-0016 X4). Newtype over a
/// `Cow<'static, str>` so a `RuleDefinition` can declare its machine with a
/// compile-time `&'static str` literal (`MachineId::new`), while a replicated
/// `Call` body deserialises the same type into an owned string. Used both as a
/// static rule-declaration column and as the [`Call::sm_cursors`] map key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineId(pub Cow<'static, str>);

impl MachineId {
    /// Construct from a compile-time literal (usable in `const`/`static`).
    pub const fn new(s: &'static str) -> Self {
        MachineId(Cow::Borrowed(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A state label within a machine (ADR-0016 X4). Newtype over a
/// `Cow<'static, str>` for the same reason as [`MachineId`]: static rule
/// declarations carry `&'static str` literals; replicated cursors deserialise
/// into owned strings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateLabel(pub Cow<'static, str>);

impl StateLabel {
    /// Construct from a compile-time literal (usable in `const`/`static`).
    pub const fn new(s: &'static str) -> Self {
        StateLabel(Cow::Borrowed(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The **terminal** sentinel (ADR-0016 X9 machine deactivation) — a declared
    /// transition `S => terminal` means a rule deactivates the machine from `S`
    /// (removing its cursor). Rendered as Mermaid's `[*]` sink; never a real
    /// cursor value (a deactivated machine has *no* cursor).
    pub const fn terminal() -> Self {
        StateLabel(Cow::Borrowed("[*]"))
    }
    /// Is this the [`terminal`](Self::terminal) sentinel?
    pub fn is_terminal(&self) -> bool {
        self.0 == "[*]"
    }
}

// ── INVITE client transaction handle (in-memory; opaque) ────────────────────

/// Host/port destination of an in-flight INVITE transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

/// Handle for an in-flight INVITE client transaction. Carries enough to rebuild
/// the CANCEL (§9.1 branch reuse) / ACK-for-2xx (§13.2.2.4 CSeq) wire form.
///
/// The source kept `originalInvite` as `Schema.Unknown` (best-effort through
/// Redis); here it is stored as the raw INVITE **bytes**, so the call crate
/// stays a pure leaf with no `sip-message` dependency. The source's constant
/// `kind: "invite"` discriminant is dropped (the type is the discriminant).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTxnHandle {
    pub branch: String,
    #[serde(with = "serde_bytes")]
    pub original_invite: Vec<u8>,
    pub destination: HostPort,
}

// ── Remote address ──────────────────────────────────────────────────────────

/// Remote peer endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub address: String,
    pub port: u16,
}

// ── Pending transparent-relay request ───────────────────────────────────────

/// Direction of an original relayed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    FromA,
    FromB,
}

/// Snapshot of an inbound request the B2BUA relays transparently, stored on the
/// target-leg dialog so its response can be rebuilt with the right
/// Via/From/To/Call-ID/CSeq (RFC 3261 §8.1.3.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub method: String,
    pub outbound_cseq: i64,
    pub inbound_cseq: i64,
    pub source_vias: Vec<String>,
    pub source_call_id: String,
    pub source_from: String,
    pub source_to: String,
    pub direction: Direction,
}

// ── Dialog ──────────────────────────────────────────────────────────────────

/// RFC 3261 §12 dialog state, stack-owned. `localTag` is the B2BUA's tag on this
/// leg; `remoteTag` is the peer's. `callId`/`localUri`/`remoteUri` are
/// denormalised from the enclosing leg so generators need no leg context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDialog {
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
    pub local_uri: String,
    pub remote_uri: String,
    /// Peer Contact URI — Request-URI for in-dialog requests (§12.2.1.1).
    pub remote_target: String,
    /// Last-sent CSeq on this dialog (§8.1.1.5).
    pub local_cseq: i64,
    /// Outbound route set from the dialog-creating response (§12.1.2).
    pub route_set: Vec<String>,
}

/// B2BUA-only dialog extensions that never surface to the SIP stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct B2buaDialogExt {
    /// Remote party's highest CSeq. `None` until first message from remote.
    pub remote_cseq: Option<i64>,
    /// Pending transparently-relayed inbound requests awaiting response here.
    pub inbound_pending_requests: Vec<PendingRequest>,
    /// Via branch of the first ACK for this dialog's 2xx (§13.2.2.4 re-ACK).
    pub ack_branch: Option<String>,
    /// In-flight re-INVITE client transaction handle on this dialog.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// SDP cached from a reliable 18x / UPDATE under the `fake-prack` strategy.
    #[serde(with = "serde_bytes")]
    pub cached_sdp: Option<Vec<u8>>,
}

/// Composite Dialog = stack §12 state + B2BUA-only extensions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dialog {
    pub sip: StackDialog,
    pub ext: B2buaDialogExt,
}

// ── Tag mapping (B-leg remote tag ↔ B2BUA tag shown to A-leg) ───────────────

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

// ── Leg state & disposition ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegState {
    Trying,
    Early,
    Confirmed,
    Terminated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegDisposition {
    Pending,
    Bridged,
    Cancelling,
    Rejected,
}

/// Per-leg BYE disposition — how each leg was (or will be) torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeDisposition {
    /// We sent BYE, awaiting 200 OK or timeout (non-terminal).
    ByeSent,
    /// Remote sent BYE to us (we already replied 200).
    ByeReceived,
    /// 200 OK received for our outbound BYE.
    ByeConfirmed,
    /// BYE transaction timed out (far side unresponsive).
    ByeTimeout,
    /// CANCEL sent (pre-dialog, no BYE needed).
    Cancelled,
    /// Far side rejected INVITE (4xx/5xx/6xx, no BYE needed).
    Rejected,
    /// Leg never established (e.g. failover replaced it).
    None,
}

impl ByeDisposition {
    /// Terminal dispositions — no more SIP traffic expected for this leg.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ByeDisposition::ByeConfirmed
                | ByeDisposition::ByeReceived
                | ByeDisposition::ByeTimeout
                | ByeDisposition::Cancelled
                | ByeDisposition::Rejected
                | ByeDisposition::None
        )
    }
}

/// Explicit per-leg role (ADR-0014). Read via [`crate::helpers::leg_kind`],
/// which defaults from `legId` when absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegKind {
    A,
    Destination,
    Media,
    TransferTarget,
}

// ── Leg ─────────────────────────────────────────────────────────────────────

/// Per-leg state. `legId` is `"a"`, `"b-1"`, `"b-2"`, …
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub leg_id: String,
    pub call_id: String,
    pub from_tag: String,
    pub source: RemoteInfo,
    pub state: LegState,
    pub disposition: LegDisposition,
    /// Multiple during early state (forking); one survives after confirmed.
    pub dialogs: Vec<Dialog>,
    pub no_answer_timeout_sec: Option<i64>,
    /// How this leg was torn down. `None` while the call is active.
    pub bye_disposition: Option<ByeDisposition>,
    /// B2BUA's local URI for this leg (From for outbound requests).
    pub local_uri: Option<String>,
    /// Remote party's URI for this leg (To for outbound requests).
    pub remote_uri: Option<String>,
    /// Request-URI of the outbound INVITE — needed for CANCEL (§9.1).
    pub invite_request_uri: Option<String>,
    /// In-flight initial-INVITE client transaction handle on this leg.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// Per-service opaque extension slot (ADR-0016).
    pub ext: Option<ExtMap>,
    /// Explicit leg role (ADR-0014); read via [`crate::helpers::leg_kind`].
    pub kind: Option<LegKind>,
    /// Whether generic relay/keepalive rules own this leg; read via
    /// [`crate::helpers::is_adopted`].
    pub adopted: Option<bool>,
}

// ── Timer entry (serializable intent, not a runtime fiber) ──────────────────

/// Closed union of known timer types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerType {
    NoAnswer,
    /// Call-level a-leg setup deadline: armed at route time, cancelled at
    /// answer, untouched by reroutes (each reroute gets its own per-leg
    /// `NoAnswer`; this caps the *whole* setup). Rides the replicated
    /// `call.timers` ledger, so unlike the sip-txn `INVITE_INITIAL_TIMEOUT`
    /// backstop (which dies with a crashed node's transactions) it survives
    /// crash → reclaim and still reaps a stuck-in-setup call.
    SetupTimeout,
    GlobalDuration,
    LimiterRefresh,
    Keepalive,
    KeepaliveTimeout,
    /// RFC 3261 §13.3.1.4 — periodic retransmit of the a-leg INVITE **2xx** while
    /// the caller's ACK is missing (re-armed each fire; cancelled on the a-leg
    /// ACK). Paired with [`AckTimeout`](Self::AckTimeout), which bounds the window.
    AckRetransmit,
    /// RFC 3261 §13.3.1.4 — the a-leg 2xx-without-ACK **give-up** deadline (RFC's
    /// 64·T1). Single-shot, armed when the a-leg 2xx is relayed at dialog
    /// confirmation, cancelled on the a-leg ACK; on expiry the B2BUA BYEs the
    /// just-created a-leg dialog AND tears down the b-leg (the answered-call leak
    /// fix). Distinct from [`AckRetransmit`](Self::AckRetransmit), the on-wire
    /// re-send cadence.
    AckTimeout,
    /// Safety-net timer scheduled when entering "terminating" state.
    TerminatingTimeout,
    /// REFER subscription expiry (RFC 3515).
    ReferSubscriptionExpiry,
    /// Per re-INVITE answer watchdog during REFER-driven blind transfer.
    ReferReinviteAnswer,
    /// Overall REFER safety timer covering the full transfer state machine.
    ReferOverallSafety,
}

/// A serializable timer intent (the live fiber lives in the deferred
/// `TimerService` slice).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub timer_type: TimerType,
    /// Epoch ms — absolute deadline.
    pub fire_at: i64,
    /// `None` = call-level timer.
    pub leg_id: Option<String>,
}

// ── Call limiter state ──────────────────────────────────────────────────────

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

// ── CDR event ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CdrEventType {
    InviteReceived,
    InviteSent,
    Provisional,
    Answer,
    Bye,
    Cancel,
    Timeout,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdrEvent {
    #[serde(rename = "type")]
    pub event_type: CdrEventType,
    pub timestamp: i64,
    pub leg_id: String,
    pub status_code: Option<i64>,
    pub reason: Option<String>,
}

// ── A-leg INVITE snapshot (for failover b-leg reconstruction) ───────────────

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

// ── Rule system ─────────────────────────────────────────────────────────────

/// A rule activated on this call by the HTTP API response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRule {
    /// Rule identifier — matches a registered RuleDefinition id.
    pub id: String,
    /// Whether currently active (can be deactivated mid-call).
    pub active: bool,
}

// ── Call state ──────────────────────────────────────────────────────────────

/// Call lifecycle: `active` → `terminating` (BYEs sent, awaiting resolution) →
/// `terminated` (removable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallModelState {
    Active,
    Terminating,
    Terminated,
}

// ── HA topology hint ────────────────────────────────────────────────────────

/// Persisted topology hint: the worker pair stamped at INVITE time plus a
/// per-context **version vector** `(gen, bak_gen)` = `(p, b)` (ADR-0014).
///
/// `gen` (`p`) is the **primary** counter — bumped only by the call's primary on
/// a local mutation. `bak_gen` (`b`) is the **backup** counter — bumped only by
/// an acting-backup on a takeover mutation. Each node bumps only its own role's
/// counter, so the *other* node's counter in an incoming update is, by
/// construction, the branch point. This replaces the old single-counter
/// "highest gen wins" LWW, which could not disambiguate concurrent
/// primary+backup mutations (the latent equal-gen divergence). See the apply
/// rule in `b2bua::repl::puller`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTopology {
    pub pri: String,
    pub bak: String,
    /// Primary counter `p` of the `(p,b)` version vector.
    pub gen: i64,
    /// Backup counter `b` of the `(p,b)` version vector. `#[serde(default)]` so
    /// a body serialised before the split still deserialises (`b = 0`).
    #[serde(default)]
    pub bak_gen: i64,
}

// ── Active peering (INAP-style split/merge) ─────────────────────────────────

/// The single active leg pair (1↔1). `None` on the [`Call`] means 1↔0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePeer {
    pub leg_a: String,
    pub leg_b: String,
}

// ── Policy body override (the one preserved absent/null/value distinction) ───

/// Body override derived from active features. Wrapped in `Option` on the
/// [`Call`]: `None` = no override (absent), `Some(Empty)` = force empty body
/// (the source's `null`), `Some(Bytes)` = substitute this body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyUpdateBody {
    Empty,
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
}

// ── Call ─────────────────────────────────────────────────────────────────────

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
    /// The source's `optional(NullOr(String))` collapses to `Option` here
    /// (absent and null both mean "no attribution").
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
    /// HA topology hint (source field `_topology`).
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
    /// Per-call message counter for cap-defense (omitted from the wire as 0 in
    /// the source; always present here, defaulting to 0).
    pub message_count: Option<i64>,
    /// Leg IDs that already triggered one safety-timer refresh while terminating.
    pub terminating_refresh_legs: Option<Vec<String>>,
    /// Per-call runtime state for the `relayFirst18xTo180` service (the typed
    /// slice that replaces the TS shared `ruleState` blob; ADR-0016's full
    /// typed-ext is out of scope for the early port). `None` until the first
    /// 18x is processed under an active strategy.
    pub relay_first_18x: Option<RelayFirst18xState>,
    /// Per-call runtime state for the `promote18xPemTo200` service (strategy
    /// `promote-pem-to-200`). Mirrors the TS `PemCallExt`; the typed slice
    /// replaces the shared ext blob (ADR-0016 full typed-ext is out of scope for
    /// the early port). `None` until the first 183+PEM is promoted.
    pub promote_pem: Option<PromotePemState>,
    /// Per-call runtime state for the REFER blind-transfer service. Mirrors the
    /// TS `TransferCallExt` (`referTransfer.ts`); the typed slice replaces the
    /// `defineService` call-ext blob (ADR-0016 full typed-ext is out of scope
    /// for the early port). `None` until a REFER is intercepted; presence is the
    /// service-activation guard. Cleared (`None`) on every terminal transition.
    pub transfer: Option<TransferState>,
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

/// REFER blind-transfer phase — the authoritative state of the `transfer`
/// callflow machine (ADR-0016 slice 7). It is projected into the per-call
/// `transfer` machine cursor (`refer_transfer::project_cursor`), and each
/// transfer rule is gated by that cursor via its `active_states`. Port of the TS
/// `TransferPhase` literal union (`referTransfer.ts:38-47`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferPhase {
    /// REFER received, awaiting the HTTP authorization decision.
    ReferAuthorizing,
    /// C-leg INVITE sent, awaiting its final response.
    CRinging,
    /// Re-INVITE toward C with A's SDP in flight (slice 5b+).
    CRealigning,
    /// Re-INVITE toward A with C's endpoint in flight (slice 5b+).
    ARealigning,
}

/// Per-call REFER transfer state. Holds the phase + addressable leg ids (actions
/// and filters address legs by id) and the payload carried across phases. Port
/// of the TS `TransferCallExt` (`referTransfer.ts:56-85`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferState {
    /// Current transfer phase.
    pub phase: TransferPhase,
    /// B-leg that issued the REFER (origin of the implicit subscription).
    pub referrer_leg_id: String,
    /// Raw Refer-To URI as received from the referrer.
    pub refer_to_uri: String,
    /// Refer-To URI after any HTTP-driven rewrite (`new_refer_to`).
    pub effective_refer_to_uri: Option<String>,
    /// Callback context propagated from the /call/refer response.
    pub callback_context: Option<String>,
    /// Newly-created C-leg identifier (set when create-leg fires).
    pub c_leg_id: Option<String>,
    /// CSeq of the REFER request on the referrer's dialog (NOTIFY correlation).
    pub refer_cseq: Option<u32>,
    /// Wall-clock ms when the REFER was received.
    pub started_at_ms: i64,
    /// Last 1xx status forwarded on the subscription as a NOTIFY sipfrag — used
    /// by `transfer-c-1xx-to-notify` to dedupe repeats.
    pub last_c_leg_notified_status: Option<u16>,
    /// C-leg's initial 200-OK answer SDP, carried across `c-realigning` so the
    /// a-realign re-INVITE (slice 5b) can offer it back to A.
    #[serde(with = "serde_bytes")]
    pub c_initial_sdp: Option<Vec<u8>>,
}

/// Runtime state for the `relayFirst18xTo180` service. Strategy itself lives on
/// `features.relay_first_18x_to_180`; this carries the per-call progress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayFirst18xState {
    /// Whether the first 18x has been relayed as a bare 180 to the caller.
    pub first_relayed: bool,
    /// The a-facing To-tag minted on the first 18x — reused on the 200 OK so the
    /// caller sees one stable callee identity across forking/failover.
    pub stored_a_tag: Option<String>,
}

/// Runtime state for the `promote18xPemTo200` service (port of the TS
/// `PemCallExt`). Strategy itself lives on `features.relay_first_18x_to_180`;
/// this carries the per-call promotion progress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotePemState {
    /// True once the first 183+SDP+PEM has been promoted to a synthetic 200 OK.
    pub promoted: bool,
    /// SDP sent to Alice in the synthetic 200 OK; compared against B's final
    /// answer to decide whether a resync re-INVITE toward Alice is needed.
    #[serde(with = "serde_bytes")]
    pub promoted_sdp: Vec<u8>,
    /// While true, Alice's in-dialog requests (other than BYE) are rejected.
    pub window_open: bool,
    /// CSeq of an outstanding B2BUA-originated resync re-INVITE toward Alice.
    pub resync_reinvite_cseq: Option<i64>,
}
