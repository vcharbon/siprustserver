//! Request/response shapes for the call-decision adapter — port of
//! `src/decision/schemas/*`. `FeatureActivations` is reused from `call` (it is
//! the canonical feature shape; the data model already carries it on `Call`).
//!
//! Only [`NewCallRequest`] derives serde — it is what the future HTTP adapter
//! POSTs and what the scripted test adapter inspects. The response types are
//! constructed in-process (by the scripted adapter / a future JSON decoder), so
//! they stay plain Rust values here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use call::features::FeatureActivations;

/// Header name → value, or `None` to delete the header.
pub type SipHeaderUpdates = BTreeMap<String, Option<String>>;

/// The decision request sent on a new INVITE (the call context the backend
/// keys decisions off — R-URI, From/To, all non-structural `X-*` headers, body).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewCallRequest {
    pub call_id: String,
    pub ruri: String,
    pub from: String,
    pub to: String,
    pub via: Vec<String>,
    /// Every `Contact` line, in wire order. A `Contact` is repeatable
    /// (RFC 3261 §20.10) and diversion/redirect flows can carry several — keep
    /// each instance rather than collapsing to the last.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub contact: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_type: Option<String>,
    /// All headers not sent as a top-level field (this is where `X-*` land).
    ///
    /// Multi-valued: a header appearing on N separate lines keeps **all N**
    /// values, in wire order, under its name. `History-Info`, `Diversion`, and
    /// `P-Asserted-Identity` are multi-hop/multi-instance by nature — collapsing
    /// them to the last line silently drops every earlier hop before the
    /// decision engine sees them. Use [`NewCallRequest::sip_header`] for the
    /// common single-value read.
    #[serde(default)]
    pub sip_headers: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sip_body: Option<String>,
}

impl NewCallRequest {
    /// First value of a repeatable header, for the common case where a consumer
    /// wants a single value (e.g. `X-Api-Call`). Returns `None` if the header is
    /// absent or present with no value. For every instance, index `sip_headers`
    /// directly.
    pub fn sip_header(&self, name: &str) -> Option<&str> {
        self.sip_headers.get(name).and_then(|v| v.first()).map(String::as_str)
    }
}

/// A downstream SIP peer.
#[derive(Debug, Clone)]
pub struct SipDestination {
    pub host: String,
    pub port: Option<u16>,
    pub transport: Option<String>,
}

impl SipDestination {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port: Some(port),
            transport: None,
        }
    }
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(5060)
    }
}

/// Three-way body directive on a route: leave the inbound body, drop it, or
/// substitute a new one (the source's `update_body` absent/null/value).
#[derive(Debug, Clone, Default)]
pub enum BodyUpdate {
    #[default]
    Keep,
    Drop,
    Replace(String),
}

#[derive(Debug, Clone)]
pub struct CallLimiterEntry {
    pub id: String,
    pub limit: i64,
}

/// A "route" decision — bridge the call to `destination`.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub destination: SipDestination,
    pub new_ruri: Option<String>,
    /// Rewrite the outbound b-leg **From URI** (the "from number"). The B2BUA
    /// always owns the From *tag* (ADR-0017 header-ownership matrix) — only the
    /// URI is HTTP-settable. `None` keeps the relayed a-leg From URI.
    pub new_from: Option<String>,
    /// Rewrite the outbound b-leg **To URI** (the "to number"). The To tag stays
    /// B2BUA-owned. `None` keeps the relayed a-leg To URI.
    pub new_to: Option<String>,
    pub update_headers: Option<SipHeaderUpdates>,
    pub update_body: BodyUpdate,
    pub no_answer_timeout_sec: Option<i64>,
    pub call_limiter: Vec<CallLimiterEntry>,
    pub callback_context: Option<String>,
    pub features: FeatureActivations,
    pub service_ext: BTreeMap<String, serde_json::Value>,
    /// Internal release events the backend wants consulted on (`call_release`)
    /// instead of handled locally — the Routing API's `subscribe[]`
    /// (newkahneed-009). Recorded on the call at route-apply time (initial
    /// route: `apply_route`; async failover/reroute route: the
    /// `SetSubscriptions` fold), so it survives replication/takeover like
    /// `features`. Empty = no subscriptions = today's local handling. (This
    /// in-process type carries no serde; the persistence/back-compat default
    /// lives on `Call.subscriptions`, which is `#[serde(default)]`.)
    pub subscriptions: Vec<call::ReleaseEventKind>,
}

/// A "reject" decision — answer the INVITE with a failure response the decision
/// layer authors (code + reason-phrase + extra headers, e.g. `Reason:`).
#[derive(Debug, Clone)]
pub struct RejectDecision {
    pub reject_code: u16,
    pub reject_reason: Option<String>,
    pub update_headers: Option<SipHeaderUpdates>,
}

/// One redirect target in a [`RedirectDecision`]'s Contact list. `q` is the
/// advisory caller-side preference (RFC 3261 §20.10); rendered verbatim in list
/// order (the platform does not reorder — ADR-0017 X5).
#[derive(Debug, Clone)]
pub struct RedirectContact {
    pub uri: String,
    pub q: Option<f32>,
}

/// A "redirect" decision — answer the caller with a 3xx (default 302) carrying an
/// ordered Contact list. Contact is the one header the decision layer owns *only*
/// on a redirect (ADR-0017 header-ownership matrix).
#[derive(Debug, Clone)]
pub struct RedirectDecision {
    /// 3xx status (300/301/302/305); defaults to 302 when built via helpers.
    pub code: u16,
    pub reason: Option<String>,
    pub contacts: Vec<RedirectContact>,
    pub update_headers: Option<SipHeaderUpdates>,
}

/// The single **call treatment** the decision layer returns — at *every* hop
/// (new-call and failover) it draws from the same closed set (ADR-0017 X1):
///   - `Route`    — bridge to a destination, with identity/header rewrites;
///   - `Redirect` — emit a 3xx to the caller with a Contact list;
///   - `Reject`   — author a final failure response (code/reason/headers);
///   - `Relay`    — pass the last attempted b-leg's failure response verbatim
///     (only meaningful on the failover path; with no captured failure it falls
///     back to 480, ADR-0017 X5).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Route is the hot path; boxing adds an alloc per call.
pub enum CallTreatment {
    Route(RouteDecision),
    Redirect(RedirectDecision),
    Reject(RejectDecision),
    Relay,
}

/// Response to a new INVITE. An alias for [`CallTreatment`] — `new_call` returns
/// `Route | Redirect | Reject` (`Relay` is failover-only and treated as an error
/// if returned here).
pub type NewCallResponse = CallTreatment;

/// Response to a b-leg failure. An alias for [`CallTreatment`] — any of the four
/// variants is valid on the failover path.
pub type CallFailureResponse = CallTreatment;

/// What failed on a b-leg, for the failover decision. Carries the
/// **event-scoped** facts only — everything the emitting site alone knows.
/// Call-scoped context rides the [`CallSnapshot`] the framework attaches.
#[derive(Debug, Clone, Default)]
pub struct FailureInfo {
    pub origin: String,
    pub status_code: Option<u16>,
    pub limiter_id: Option<String>,
    /// The leg whose failure triggered this decision (`None` for pre-leg
    /// origins such as a limiter reject).
    pub failed_leg_id: Option<String>,
    /// The failed final response's non-structural headers, verbatim and in
    /// wire order (duplicates preserved) — where `Reason:`/`Warning:`/`X-*`
    /// land. Empty for internal origins (timeouts, limiter).
    pub sip_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct CallFailureRequest {
    pub callback_context: Option<String>,
    pub failure: FailureInfo,
    /// Call-scoped context, attached by the framework at dispatch time.
    pub snapshot: CallSnapshot,
}

// ── Call-context snapshot (attached to every decision-point request) ────────

/// Read-only per-leg view inside a [`CallSnapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct LegSnapshot {
    pub leg_id: String,
    pub state: call::LegState,
    pub disposition: call::LegDisposition,
    /// How the leg was (or is being) torn down; `None` while it is live.
    pub bye_disposition: Option<call::ByeDisposition>,
    /// The B2BUA's local URI on this leg.
    pub local_uri: Option<String>,
    /// The remote party's URI on this leg.
    pub remote_uri: Option<String>,
    /// Request-URI of the outbound INVITE (post-rewrite).
    pub invite_request_uri: Option<String>,
    /// Per-service opaque leg ext slices (ADR-0016).
    pub ext: Option<call::ExtMap>,
}

/// Read-only snapshot of the **observed call context**, built by the framework
/// from the authoritative `Call` at dispatch time and attached to every
/// decision-point request that has a call behind it ([`CallFailureRequest`],
/// [`CallReferRequest`]). One generic carrier instead of one upstream field per
/// downstream need: a decision backend derives what it wants (a `prov18x`
/// flag is "any `Provisional` event ≥ 180 on the failed leg", ringing duration
/// is two timestamps, REFER authorization can read per-service state) without
/// the platform learning any of those semantics. Cost: one clone per
/// failure/refer callback — never per SIP message.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallSnapshot {
    /// A-leg Call-ID (correlation handle for the decision backend).
    pub call_id: String,
    pub callback_context: Option<String>,
    /// Every leg, a-leg first, in attempt order.
    pub legs: Vec<LegSnapshot>,
    /// The full observed CDR trail (invite/provisional/answer/reject/timeout…).
    pub cdr_events: Vec<call::CdrEvent>,
    /// Per-service opaque call ext slices (`Call.ext`, ADR-0016).
    pub service_ext: call::ExtMap,
    /// Per-service state-machine cursors (machine id → state label).
    pub sm_cursors: BTreeMap<String, String>,
    /// What the routing decision activated.
    pub features: Option<FeatureActivations>,
    /// Live limiter holds on the call.
    pub limiter_ids: Vec<String>,
}

impl CallSnapshot {
    /// Snapshot `call` for a decision request. Framework-only: rules never
    /// build one (they pass event-scoped facts; the router attaches this).
    pub fn of(call: &call::Call) -> Self {
        let legs = std::iter::once(&call.a_leg)
            .chain(call.b_legs.iter())
            .map(|l| LegSnapshot {
                leg_id: l.leg_id.clone(),
                state: l.state,
                disposition: l.disposition,
                bye_disposition: l.bye_disposition,
                local_uri: l.local_uri.clone(),
                remote_uri: l.remote_uri.clone(),
                invite_request_uri: l.invite_request_uri.clone(),
                ext: l.ext.clone(),
            })
            .collect();
        CallSnapshot {
            call_id: call.a_leg.call_id.clone(),
            callback_context: call.callback_context.clone(),
            legs,
            cdr_events: call.cdr_events.clone(),
            service_ext: call.ext.clone().unwrap_or_default(),
            sm_cursors: call
                .sm_cursors
                .iter()
                .map(|(m, s)| (m.as_str().to_string(), s.as_str().to_string()))
                .collect(),
            features: call.features.clone(),
            limiter_ids: call.limiter_entries.iter().map(|e| e.limiter_id.clone()).collect(),
        }
    }
}

/// The release-event consult sent when a **subscribed** internal release
/// event fires (newkahneed-009; the Routing API's `POST /calls/events/release`).
/// Built by the `max-duration` rule's `ReleaseAsyncHttp` seed; the framework
/// attaches the snapshot at dispatch, exactly like [`CallFailureRequest`].
#[derive(Debug, Clone)]
pub struct CallReleaseRequest {
    pub callback_context: Option<String>,
    /// Which subscribed internal event fired.
    pub event: call::ReleaseEventKind,
    /// Call-scoped context, attached by the framework at dispatch time.
    pub snapshot: CallSnapshot,
}

/// The release decision: proceed with the local teardown, or convert the
/// release into an **established-call reroute** (replace the connected b-leg
/// with a new destination — announcement / autocutoff treatment). The `Route`
/// here is the same [`RouteDecision`] shape as every other decision point
/// (ADR-0017 one-treatment-vocabulary; output parity per newkahneed-005
/// applies: `call_limiter` admission, `features`, `service_ext`,
/// `subscriptions` are all honored on the reroute).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // mirror of CallTreatment's choice
pub enum CallReleaseResponse {
    /// Tear the call down locally, exactly as an unsubscribed event would.
    Release,
    /// Reroute the established call to `RouteDecision::destination`.
    Route(RouteDecision),
}

/// The REFER authorization request POSTed to `/call/refer` (port of TS
/// `CallReferRequest`, requests.ts:57-64). Built by `transfer-intercept-refer`.
#[derive(Debug, Clone, Default)]
pub struct CallReferRequest {
    /// A-leg Call-ID.
    pub call_id: String,
    /// `Call-ID;to-tag=…;from-tag=…` from the referrer (B) leg's perspective.
    pub dialog_id: String,
    pub callback_context: Option<String>,
    pub refer_to: String,
    pub referred_by: Option<String>,
    /// Non-structural REFER headers forwarded verbatim (incl. `X-Api-Call`).
    pub sip_headers: BTreeMap<String, String>,
    /// Call-scoped context, attached by the framework at dispatch time.
    pub snapshot: CallSnapshot,
}

#[derive(Debug, Clone)]
pub enum CallReferResponse {
    Allow {
        destination: SipDestination,
        /// Rewritten Refer-To URI (`new_refer_to`).
        new_refer_to: Option<String>,
        /// Header set/remove directives applied to the C INVITE.
        update_headers: Option<SipHeaderUpdates>,
        /// No-answer timeout for the C leg (seconds).
        no_answer_timeout_sec: Option<i64>,
        /// Callback context propagated onto the transfer slice.
        callback_context: Option<String>,
    },
    Reject {
        code: u16,
        reason: Option<String>,
    },
}

/// Mandatory platform features every successful route must carry (the source
/// 500s if `features` is absent). A sane default for the scripted adapter.
pub fn default_platform_features() -> FeatureActivations {
    use call::features::{KeepaliveActivation, PlatformActivations};
    FeatureActivations {
        platform: PlatformActivations {
            max_duration_sec: 3_600,
            keepalive: KeepaliveActivation {
                interval_sec: 30,
                max_missed: 2,
            },
        },
        refer: None,
        relay_first_18x_to_180: None,
        no_answer_timeout_sec: None,
        call_limiters: None,
    }
}
