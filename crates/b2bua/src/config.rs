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
    /// In-dialog OPTIONS keepalive interval, seconds. The B2BUA arms a keepalive
    /// timer at dialog confirmation and re-arms it each cycle; on expiry it pokes
    /// every peered leg with an in-dialog OPTIONS. Production default is **300 s**
    /// (operator: "in-call OPTIONS every 5 minutes"); a shorter interval (e.g.
    /// 30 s) breaks long-hold endurance traffic by poking mid-dialog calls whose
    /// UAC is not expecting it. Overridable per worker via `B2BUA_KEEPALIVE_SEC`.
    /// The test harness lowers this to 30 s so paused-clock tests stay fast.
    pub keepalive_interval_sec: i64,
    /// **Reboot budget**, seconds — the TTL stamped on every *replicated* backup
    /// `Element` (ADR-0011 X11). It is how long a backup copy survives without a
    /// refresh from its primary, i.e. how long a primary may be down/rebooting
    /// before its backups give up and self-evict. Decoupled from the OPTIONS
    /// keepalive (which is leg-liveness, a different concern); default **450 s**.
    ///
    /// Correctness coupling — the backup's TTL is *refreshed* only when the
    /// primary flushes the call, and a quiescent established call is flushed only
    /// by its keepalive OPTIONS. So this budget MUST outlast one keepalive gap
    /// (`reboot_budget_sec >= keepalive_interval_sec`) or a healthy-but-idle
    /// call's backup expires between pokes, silently dropping its failover
    /// coverage. [`validate`](Self::validate) enforces both this and the absolute
    /// floors. The non-replicating path ignores this (TTL stays `CALL_TTL_MS`).
    pub reboot_budget_sec: i64,
    /// Limiter-refresh cadence, seconds — how often an admitted call migrates its
    /// holds to the current window so a long call never ages out of the summed
    /// lookback. Must match the limiter service's `LIMITER_WINDOW_SECONDS`. TS
    /// default 300. The test harness lowers this for fast paused-clock tests.
    pub limiter_refresh_sec: i64,
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
            keepalive_interval_sec: 300,
            reboot_budget_sec: 450,
            limiter_refresh_sec: 300,
        }
    }
}

impl B2buaConfig {
    /// Absolute minimum OPTIONS keepalive (s). Below 2 min a mid-dialog OPTIONS
    /// poke breaks long-hold traffic (see [`keepalive_interval_sec`] doc). A
    /// **production** floor only — the paused-clock test harness builds configs
    /// directly and skips [`validate`](Self::validate) to use a faster cadence.
    pub const MIN_KEEPALIVE_SEC: i64 = 120;
    /// Absolute minimum reboot budget (s): a backup must survive a primary's
    /// reboot. The effective floor is usually higher — see [`validate`].
    pub const MIN_REBOOT_BUDGET_SEC: i64 = 60;

    /// Validate operator-supplied tunables at **boot** (the runner calls this and
    /// refuses to start on `Err`; unit/sim harnesses construct configs directly
    /// and skip it). Returns the first violation as a human-readable message.
    pub fn validate(&self) -> Result<(), String> {
        if self.keepalive_interval_sec < Self::MIN_KEEPALIVE_SEC {
            return Err(format!(
                "keepalive_interval_sec={} < min {} s (2 min): a shorter in-dialog \
                 OPTIONS cadence breaks long-hold traffic",
                self.keepalive_interval_sec, Self::MIN_KEEPALIVE_SEC
            ));
        }
        if self.reboot_budget_sec < Self::MIN_REBOOT_BUDGET_SEC {
            return Err(format!(
                "reboot_budget_sec={} < min {} s (1 min): a replicated backup must \
                 survive a primary reboot",
                self.reboot_budget_sec, Self::MIN_REBOOT_BUDGET_SEC
            ));
        }
        // The backup `Element` TTL is refreshed only on a primary flush, and a
        // quiescent established call is flushed only by its keepalive OPTIONS. So
        // the budget must outlast one keepalive gap or an idle call's backup
        // self-evicts before its next refresh, silently losing failover coverage.
        if self.reboot_budget_sec < self.keepalive_interval_sec {
            return Err(format!(
                "reboot_budget_sec={} < keepalive_interval_sec={}: the backup TTL is \
                 refreshed each keepalive flush, so the budget must outlast one \
                 keepalive gap (an idle call is flushed only by its keepalive)",
                self.reboot_budget_sec, self.keepalive_interval_sec
            ));
        }
        Ok(())
    }
}
