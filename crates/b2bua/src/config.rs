//! B2BUA runtime configuration — the subset of the source `AppConfig` the
//! dispatcher / router / store / rules read. Behavioural timeouts that have a
//! tokio analogue stay here as plain values.

/// Tunables for a B2BUA worker. Cheap to clone (a handful of scalars + two
/// short strings); share one instance across the stack.
#[derive(Clone, Debug)]
pub struct B2buaConfig {
    /// This worker's ordinal, encoded into `callRef` for partition routing.
    pub self_ordinal: String,
    /// Local signaling IP stamped into Via / Contact.
    pub sip_local_ip: String,
    /// Local signaling port stamped into Via / Contact.
    pub sip_local_port: u16,
    /// When the worker is deployed behind the SIP front proxy, every b-leg
    /// (worker→callee) outbound request is sent to this `(host, port)` with a
    /// preloaded `Route: <sip:host:port;lr;outbound>` so the proxy classifies
    /// the flow as worker-outbound (skip LB, forward to the R-URI). The R-URI /
    /// remote target stays the callee (RFC 3261 §16.12). `None` = send b-leg
    /// traffic straight to the callee (port of `AppConfig.b2bOutboundProxy`).
    pub b2b_outbound_proxy: Option<(String, u16)>,
    /// Global cap on concurrently-running handlers across all calls.
    pub event_dispatch_concurrency: usize,
    /// Per-call queue depth (events buffered behind a busy handler).
    pub per_call_queue_depth: usize,
    /// Max number of live per-call queues (memory bound).
    pub per_call_queue_cap: usize,
    /// Auto-terminate a call after this many processed messages (loop guard).
    pub max_messages_per_call: u64,
    /// Bounded CDR submit queue; `0` disables buffering (passthrough).
    pub cdr_buffer_queue_max: usize,
    /// REFER implicit-subscription expiry (RFC 3515), seconds. Armed at REFER
    /// intercept; fires while still `refer-authorizing` (HTTP hung). TS default 60.
    pub refer_subscription_expiry_sec: i64,
    /// Per re-INVITE answer watchdog during REFER realignment, seconds. TS default 32.
    pub refer_reinvite_answer_sec: i64,
    /// Overall REFER safety timer covering the whole transfer FSM, seconds. TS default 120.
    pub refer_overall_safety_sec: i64,
}

impl Default for B2buaConfig {
    fn default() -> Self {
        Self {
            self_ordinal: "w0".to_string(),
            sip_local_ip: "127.0.0.1".to_string(),
            sip_local_port: 5060,
            b2b_outbound_proxy: None,
            event_dispatch_concurrency: 1024,
            per_call_queue_depth: 64,
            per_call_queue_cap: 200_000,
            max_messages_per_call: 5_000,
            cdr_buffer_queue_max: 1_024,
            refer_subscription_expiry_sec: 60,
            refer_reinvite_answer_sec: 32,
            refer_overall_safety_sec: 120,
        }
    }
}
