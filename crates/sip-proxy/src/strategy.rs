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
//!   Record-Route URI it inserts for dialog-creating requests (§16.6.5);
//!   [`encode_originated`](RoutingStrategy::encode_originated) the same for a
//!   request a worker originates, naming the backup the worker states.
//! - [`decode_stickiness`](RoutingStrategy::decode_stickiness) — given the parsed
//!   params off the top-Route URI (which the core just stripped per §16.4),
//!   recover the downstream target. Five outcomes, see [`DecodeResult`].

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
    /// Routed to the cookie's backup: the primary is absent or not alive. The
    /// core forwards exactly like `Forward` but counts a distinct
    /// `decode_forward_backup` decision for HA observability.
    ForwardBackup { target: ProxyAddr, is_emergency: bool },
    /// Synthesize a response (e.g. 403 on HMAC tamper).
    Reject { status: u16, reason: String },
    /// Stickiness couldn't be parsed; core falls back to `select_for_new_dialog`.
    Unknown { is_emergency: bool },
    /// The cookie verified, and neither worker it names is alive: the request
    /// is dropped and counted, never answered and never placed afresh
    /// (ADR-0050 §5). The sender's retransmission meets the next view.
    Unroutable,
}

/// `select_for_new_dialog` failure. The core maps each to a distinct 503.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SelectError {
    /// No alive target (empty registry, or all alive workers in `above_critical`
    /// band for a non-emergency request). → 503.
    #[error("no target available: {reason}")]
    NoTarget { reason: String },

    /// A target was selected, but its per-worker AIMD token bucket
    /// ([`crate::load_observer`]) is empty — the LB is rate-capping this worker
    /// to let it shed in-flight load. → 503 + `Retry-After: retry_after_sec`.
    /// Only raised for non-emergency new-dialog requests (emergency + in-dialog
    /// bypass the bucket); never for an unknown/unobserved worker (bootstrap is
    /// admit). Port of the TS `selectForNewDialog-overload` `bucket_empty` reject.
    #[error("worker {worker_id} rate-capped (bucket empty); retry after {retry_after_sec}s")]
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
    /// The request names a dialog (a To-tag) no verified cookie backs: the
    /// strategy admits it as a new dialog, under every new-call cap.
    pub admit_as_new: bool,
}

/// The routing-policy seam. `async` for symmetry with the registry the LB calls
/// (the source returns `Effect`s); `ForwardAll` is effectively synchronous.
#[async_trait]
pub trait RoutingStrategy: Send + Sync {
    /// Human-readable name for logs/metrics (e.g. "ForwardAll", "LoadBalancer").
    fn name(&self) -> &str;

    /// Pick a downstream target for a request with no usable stickiness cookie.
    async fn select_for_new_dialog(
        &self,
        msg: &SipMessage,
        opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError>;

    /// Recover the target previously encoded into the topmost Route URI's params
    /// (the core already verified the URI points at us and stripped it).
    async fn decode_stickiness(&self, route_param: &RouteParams, msg: &SipMessage) -> DecodeResult;

    /// Build the URI params to stamp into the Record-Route for dialog-creating
    /// requests the proxy places on `target`. `None` → this strategy has no
    /// per-dialog stickiness.
    fn encode_stickiness(&self, target: &ProxyAddr, msg: &SipMessage) -> Option<RouteParams>;

    /// The cookie of a request a worker originates: `origin` is that worker's
    /// address and `backup` the call's other owner, as the worker states it on
    /// its request (empty: the call has one owner). The cookie names the
    /// stated backup, never one the proxy picks, so every leg of a call names
    /// the same owners (ADR-0055 §8). `None` → this strategy has no per-dialog
    /// stickiness.
    fn encode_originated(
        &self,
        origin: &ProxyAddr,
        msg: &SipMessage,
        backup: &str,
    ) -> Option<RouteParams> {
        let _ = backup;
        self.encode_stickiness(origin, msg)
    }

    /// The worker id a cookie names as the call's primary, read without
    /// verifying it: which of several cookies on one message belongs to a
    /// given worker's leg. `None` → the cookie names no worker. Reverse-path
    /// failover consults only a cookie this names the dead worker in.
    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str>;

    /// The worker id a cookie names as the call's backup, read without
    /// verifying it. `None` → the cookie names no backup.
    fn stickiness_backup<'p>(&self, _params: &'p RouteParams) -> Option<&'p str> {
        None
    }

    /// The cookie in `params` (the params of one of the proxy's own Route
    /// URIs) as a header of the proxy's own may carry it: this strategy's own
    /// fields alone, and only when they verify for `msg`'s call. `None` →
    /// nothing to carry, which a strategy whose cookie names no worker always
    /// answers.
    fn verified_cookie(&self, _params: &RouteParams, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
}
