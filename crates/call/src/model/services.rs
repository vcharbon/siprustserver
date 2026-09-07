//! Per-service call state: the opaque [`ExtMap`] extension carry plus the
//! typed runtime slices for the built-in callflow services (relay-18x,
//! PEM promotion, REFER transfer, release-reroute, release-event
//! subscriptions). Accessors live in [`crate::helpers`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-service opaque extension carry, keyed by callflow-service id. Core never
/// interprets the values; each service decodes its own slice at the rule layer.
pub type ExtMap = BTreeMap<String, serde_json::Value>;

/// REFER blind-transfer phase — the authoritative state of the `transfer`
/// callflow machine (ADR-0016 slice 7). It is projected into the per-call
/// `transfer` machine cursor (`refer_transfer::project_cursor`), and each
/// transfer rule is gated by that cursor via its `active_states`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferPhase {
    /// REFER received, awaiting the HTTP authorization decision.
    ReferAuthorizing,
    /// C-leg INVITE sent, awaiting its final response.
    CRinging,
    /// Re-INVITE toward C with A's SDP in flight.
    CRealigning,
    /// Re-INVITE toward A with C's endpoint in flight.
    ARealigning,
}

/// Per-call REFER transfer state. Holds the phase + addressable leg ids (actions
/// and filters address legs by id) and the payload carried across phases.
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
    /// a-realign re-INVITE can offer it back to A.
    #[serde(with = "serde_bytes")]
    pub c_initial_sdp: Option<Vec<u8>>,
    /// The referrer answered a refer NOTIFY 481: the implicit subscription is
    /// over (RFC 6665 §4.4.1) and no further NOTIFY leaves on it; the transfer
    /// itself runs on to its own outcome.
    #[serde(default)]
    pub subscription_terminated: bool,
}

/// An internal release event the decision backend can subscribe to on a
/// `Route` decision. When a subscribed event fires, the core consults the
/// engine's `call_release` (release vs reroute) instead of tearing the call
/// down locally. Wire form is `snake_case` (`"max_call_duration"`), matching
/// the Routing API's `subscribe[]` names. Deliberately a closed enum, not free
/// strings: the core must know each event's firing site to honor it, so an
/// unknown name is a design change, not data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseEventKind {
    /// The `GlobalDuration` (max-call-duration) cap expired on an answered call.
    MaxCallDuration,
}

/// Established-call reroute phase — the authoritative state of the
/// `release-reroute` treatment, gating its rules the way [`TransferPhase`]
/// gates the transfer rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReroutePhase {
    /// Replacement b-leg INVITE sent, awaiting its final response.
    BLegDialing,
    /// Re-INVITE toward A with the replacement leg's answer SDP in flight.
    ARealigning,
}

/// Per-call state for an in-flight established-call reroute (a `Route`-shaped
/// `call_release` decision). Presence on the [`Call`](crate::model::Call) is
/// the activation guard for the `release-reroute` rules; cleared on completion
/// (merge + old-leg BYE) and dropped with the call on any failure teardown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerouteState {
    /// Current reroute phase.
    pub phase: ReroutePhase,
    /// The replacement b-leg created toward the reroute destination.
    pub new_leg_id: String,
    /// The previously-bridged b-leg to BYE once A is realigned (`None` for a
    /// call that had no peered b-leg — the replacement simply becomes it).
    pub old_leg_id: Option<String>,
    /// Wall-clock ms when the reroute was applied.
    pub started_at_ms: i64,
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
    /// Distinct *upstream* 18x status values already relayed (dedupe key for the
    /// `ONE_PER_VALUE` messages policy — the caller-facing wire form is always
    /// the downgraded bare 180, so dedupe must key on what the callee sent).
    /// `#[serde(default)]` keeps old replicated bodies (two-element positional
    /// encoding) decoding unchanged.
    #[serde(default)]
    pub relayed_values: Vec<u16>,
}

/// Runtime state for the `promote18xPemTo200` service (strategy
/// `promote-pem-to-200`). Strategy itself lives on
/// `features.relay_first_18x_to_180`; this carries the per-call promotion
/// progress.
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
