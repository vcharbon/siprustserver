//! [`RoutingStrategy`] — the routing-policy seam (port of `RoutingStrategy.ts`).
//!
//! The proxy *core* owns every SIP mechanic (classify, top-Route strip, Via
//! push/pop, the CANCEL/ACK LRU, forwarding). The strategy never touches message
//! bytes; the core never makes a routing-policy decision. A concrete strategy
//! plugs in three hooks:
//!
//! - [`select_for_new_dialog`](RoutingStrategy::select_for_new_dialog) — pick the
//!   downstream target for an out-of-dialog / non-sticky request. `Err` →
//!   the core synthesizes a 503.
//! - [`encode_stickiness`](RoutingStrategy::encode_stickiness) — given a chosen
//!   target, optionally produce route params the core appends to the
//!   Record-Route URI it inserts for dialog-creating requests (§16.6.5).
//! - [`decode_stickiness`](RoutingStrategy::decode_stickiness) — given the parsed
//!   params off the top-Route URI (which the core just stripped per §16.4),
//!   recover the downstream target. Four outcomes, see [`DecodeResult`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use sip_message::SipMessage;

use crate::addr::ProxyAddr;

/// Stickiness payload encoded into / decoded out of a Record-Route URI. Opaque
/// `;k=v` pairs on the wire. `BTreeMap` (ordered) so serialization is
/// deterministic. `ForwardAll` uses `{target: "host:port"}`; `LoadBalancer` uses
/// `{w_pri, w_bak, e, v, kid, sig}`.
pub type RouteParams = BTreeMap<String, String>;

/// Outcome of [`RoutingStrategy::decode_stickiness`]. Tagged so the core can
/// dispatch without casts (port of the `DecodeResult` ADT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeResult {
    /// Forward straight to that address.
    Forward { target: ProxyAddr, is_emergency: bool },
    /// Primary worker dead/draining-post-grace; routed to the cookie's backup.
    /// The core forwards exactly like `Forward` but counts a distinct
    /// `decode_forward_backup` decision for HA observability.
    ForwardBackup { target: ProxyAddr, is_emergency: bool },
    /// Synthesize a response (e.g. 403 on HMAC tamper).
    Reject { status: u16, reason: String },
    /// Stickiness couldn't be parsed; core falls back to `select_for_new_dialog`.
    Unknown { is_emergency: bool },
}

/// `select_for_new_dialog` failure. The core maps each to a distinct 503.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SelectError {
    /// No alive target (empty registry, or all alive workers in `above_critical`
    /// band for a non-emergency request). → 503.
    #[error("no target available: {reason}")]
    NoTarget { reason: String },
    /// The rendezvous winner exists but its per-worker AIMD bucket is empty. →
    /// 503 + `Retry-After`. **Dead in this slice** (the AIMD token bucket is
    /// deferred — ADR-0009 — so band-only selection never raises this); the
    /// variant + the core's `Retry-After` branch are kept so the bucket drops in
    /// later with no surface change.
    #[error("rate cap exhausted for worker {worker_id}")]
    RateCapExhausted { worker_id: String, retry_after_sec: u32 },
}

/// Options for [`RoutingStrategy::select_for_new_dialog`].
#[derive(Debug, Clone, Default)]
pub struct SelectOpts {
    /// Set by the core's cookie-decode fallback path: an in-dialog request
    /// (BYE/CANCEL/re-INVITE) on an emergency call carries no Resource-Priority
    /// on the wire, so the cookie's `e=1` flag is forwarded here to keep the
    /// request out of the (future) AIMD bucket.
    pub emergency_override: bool,
}

/// The routing-policy seam. `async` for symmetry with the registry the LB calls
/// (the source returns `Effect`s); `ForwardAll` is effectively synchronous.
#[async_trait]
pub trait RoutingStrategy: Send + Sync {
    /// Human-readable name for logs/metrics (e.g. "ForwardAll", "LoadBalancer").
    fn name(&self) -> &str;

    /// Pick a downstream target for a request with no usable stickiness cookie.
    async fn select_for_new_dialog(&self, msg: &SipMessage, opts: SelectOpts) -> Result<ProxyAddr, SelectError>;

    /// Recover the target previously encoded into the topmost Route URI's params
    /// (the core already verified the URI points at us and stripped it).
    async fn decode_stickiness(&self, route_param: &RouteParams, msg: &SipMessage) -> DecodeResult;

    /// Build the URI params to stamp into the Record-Route for dialog-creating
    /// requests. `None` → this strategy has no per-dialog stickiness.
    fn encode_stickiness(&self, target: &ProxyAddr, msg: &SipMessage) -> Option<RouteParams>;
}
