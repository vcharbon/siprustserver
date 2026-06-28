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
    /// **Keepalive-timeout grace**, seconds — how long the B2BUA waits for the
    /// in-dialog OPTIONS `200` before declaring the leg dead and tearing the call
    /// down (`keepalive-timeout` rule → BYE). Production default **32 s** (was a
    /// hard-coded 5 s). The 5 s value was unsound across a worker reboot: a
    /// reclaimed dialog re-arms its keepalive and fires OPTIONS into a path still
    /// settling (smoothed reclaim burst draining over `L_max/speedup`, the proxy
    /// EndpointSlice re-discovering the rebooted worker's new pod IP), so the
    /// round-trip can momentarily exceed 5 s and the worker BYEs thousands of
    /// healthy reclaimed long-hold calls. A generous grace rides that recovery
    /// window out; it must stay well under `keepalive_interval_sec` so two
    /// keepalives never overlap. Overridable via `B2BUA_KEEPALIVE_TIMEOUT_SEC`.
    pub keepalive_timeout_sec: i64,
    /// **Reboot budget**, seconds — the TTL stamped on every *replicated* backup
    /// `Element` (ADR-0011 X11). It is how long a backup copy survives without a
    /// refresh from its primary, i.e. how long a primary may be down/rebooting
    /// before its backups give up and self-evict. Decoupled from the OPTIONS
    /// keepalive (which is leg-liveness, a different concern); default **600 s**.
    ///
    /// Under reactive-only takeover (ADR-0014) reclaim is the sole
    /// quiescent-recovery path, so this TTL must comfortably outlast the
    /// keepalive interval plus the 120 s reboot+rehydrate+smoothed-drain slack
    /// (§3 of the reactive-takeover plan): `600 ≥ 300 + 120` with margin.
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
    /// **Keepalive catch-up speed-up** (ADR-0014, performance-only). On reboot a
    /// primary's `ReclaimAll` re-materialises its whole `pri:{self}` partition;
    /// many keepalive timers are past-due. Firing them all at once floods a
    /// freshly-rehydrated node, so the reclaim handler *smooths* the backlog: the
    /// oldest-overdue keepalive fires first and the rest are staggered to drain
    /// over `L_max / speedup` (bounded to `speedup`× the normal cadence), where
    /// `L_max` is the largest overdue gap. This is pure load management with **no**
    /// correctness role (`(p,b)` reconciliation makes any incidental keepalive
    /// overlap non-corrupting), so it carries no timing assumption. Default **10**.
    /// `<= 1` disables smoothing (every past-due keepalive fires immediately).
    pub keepalive_catchup_speedup: i64,
    /// Optional cap (seconds) on the keepalive catch-up drain window for a
    /// pathological `L_max` (a very long reboot). `None` = no cap (drain over the
    /// full `L_max / speedup`). See [`keepalive_catchup_speedup`](Self::keepalive_catchup_speedup).
    pub max_catchup_window_sec: Option<i64>,
    /// **Call reaper** master switch (ADR-0020 X1). ON by default; `false` is a
    /// debugging escape hatch only — the in-process "released exactly once, one
    /// CDR" promise does not hold without it.
    pub reaper_enabled: bool,
    /// Reaper sweep cadence, seconds (rides `tokio::time::interval` —
    /// deterministic under `start_paused` tests). Default 30 s; the sweep is
    /// one short store-lock pass, off the call path.
    pub reaper_sweep_interval_sec: i64,
    /// A live call whose **last-touched stamp** is older than this is
    /// reap-eligible (ADR-0020 X4). `0` (the default) derives
    /// `3 × keepalive_interval_sec` at spawn: every healthy call — even an idle
    /// long-hold — receives its keepalive OPTIONS `200` each interval (only
    /// **real SIP traffic** stamps the ledger; self-generated housekeeping
    /// turns like `LimiterRefresh` do not), so a stamp older than 3 intervals
    /// provably means a SIP-dead call, never quietness. An explicit value must
    /// be ≥ `2 × keepalive_interval_sec` (enforced by
    /// [`validate`](Self::validate)). Liveness derives ONLY from the stamp —
    /// never `created_at`, never timer deadlines.
    pub reaper_idle_max_sec: i64,
    /// **Setup timeout**, seconds — the call-level a-leg initial-INVITE
    /// deadline: armed at route time, cancelled at answer, deliberately NOT
    /// reset by reroute/failover (each new b-leg gets its own `NoAnswer`; this
    /// caps the caller's *total* wait for a final response). It rides the
    /// replicated `call.timers` ledger, so it survives a crash → reclaim —
    /// the sip-txn `INVITE_INITIAL_TIMEOUT` (158 s) backstop cannot (the
    /// transactions die with the node), which is how a worker kill stranded
    /// mid-setup calls holding limiter slots for the full 1 h GlobalDuration
    /// (endurance 2026-06-12). Default **150 s**: below the txn backstop so
    /// the rules path owns the teardown (408 to the caller, CANCEL to pending
    /// b-legs, obligations settled), above any sane no-answer timeout so a
    /// route-supplied `NoAnswer` still fires first. `<= 0` disables (the txn
    /// backstop and GlobalDuration remain). Overridable via
    /// `B2BUA_SETUP_TIMEOUT_SEC`.
    pub setup_timeout_sec: i64,
    /// **ACK-timeout grace**, seconds (RFC 3261 §13.3.1.4 — the 2xx-without-ACK
    /// give-up window, RFC's `64·T1` = 32 s). Armed when the a-leg 2xx is relayed
    /// at dialog confirmation; cancelled when the a-leg ACK arrives. While it is
    /// pending the B2BUA retransmits the stored 2xx toward the caller (T1,
    /// doubling, capped T2). If no ACK has arrived by this deadline the caller is
    /// presumed gone: the B2BUA BYEs the (just-created) a-leg dialog AND tears
    /// down the b-leg — without this an answered-but-un-ACKed bridged call leaks
    /// until the 1 h `GlobalDuration` cap (the a-leg INVITE server txn goes
    /// `Completed` on the 2xx and is deleted silently at Timer H — no BYE). `<= 0`
    /// disables the watchdog. Overridable via `B2BUA_ACK_TIMEOUT_SEC`.
    pub ack_timeout_sec: i64,
    /// **Tier-3 CPS token-bucket capacity** (migration/09 — port of
    /// `AppConfig.cpsBucketSize`). The hard ceiling on a *burst* of new-dialog
    /// INVITEs this worker will admit: tokens accrue at
    /// [`cps_bucket_rate`](Self::cps_bucket_rate)/s up to this cap, and the
    /// admission gate consumes one per new INVITE (emergency callers consume
    /// unconditionally, which may drive the level negative). `0` disables the
    /// hard CPS gate (every non-emergency INVITE passes the bucket — the
    /// panic-ELU backstop still applies). TS default **1000**. Overridable via
    /// `B2BUA_CPS_BUCKET_SIZE`.
    pub cps_bucket_size: u32,
    /// **Tier-3 CPS token refill rate** (tokens/sec; port of
    /// `AppConfig.cpsBucketRate`). The sustained new-INVITE rate the bucket
    /// permits once its burst capacity is drained. TS default **500**.
    /// Overridable via `B2BUA_CPS_BUCKET_RATE`.
    pub cps_bucket_rate: u32,
    /// **Tier-3 panic-ELU threshold** (`0..1`; port of
    /// `AppConfig.overloadPanicEluThreshold`, slice 7 of the overload rework).
    /// A *backstop* on the worker's OWN EWMA-smoothed Event-Loop Utilization:
    /// above it, a non-emergency new INVITE that already passed the CPS bucket
    /// is still 503'd locally, regardless of the LB's AIMD cap. The LB-side AIMD
    /// (`sip_proxy::load_observer`) is the primary control loop; this catches the
    /// cases where the LB is absent, misconfigured, or itself overloaded. Kept
    /// high so it almost never fires in normal operation. TS env default
    /// **0.75** (the `OverloadController.ts` source carries a stale `0.98`
    /// comment; the shipped `OVERLOAD_PANIC_ELU_THRESHOLD` fallback is `0.75`).
    /// `>= 1.0` effectively disables it (the clamped ELU never exceeds 1).
    /// Overridable via `B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD`.
    pub overload_panic_elu_threshold: f64,
    /// **Retry-After base** (seconds; port of `AppConfig.retryAfterBaseSec`) for
    /// the panic-ELU 503. The `bucket_empty` 503 instead derives its Retry-After
    /// from the bucket's time-to-next-token. TS default **5**. Overridable via
    /// `B2BUA_RETRY_AFTER_BASE_SEC`.
    pub retry_after_base_sec: u32,
    /// **b-leg target admission allow-list** (port of
    /// `AppConfig.workerAllowedTargetSuffixes`). The decision boundary classifies
    /// `route.destination.host` against this list (see `target_admission`): an IP
    /// literal always passes; otherwise the host must end with one of these
    /// suffixes (case-insensitive), else the gate emits a `503` and terminates the
    /// call *before* any b-leg state is allocated. This stops a bogus host (a typo,
    /// a `.svc.cluster.local` name the K8s runner constructs, a dev `/etc/hosts`
    /// entry) from reaching the send path and blocking on `getaddrinfo`/`EAI_AGAIN`.
    /// The literal `"*"` matches every host (a rollback sentinel — restores
    /// pre-admission behaviour without a redeploy). TS env default
    /// `".svc.cluster.local"` (the K8s in-cluster DNS suffix); an empty list
    /// rejects every non-IP host. Overridable via `WORKER_ALLOWED_TARGET_SUFFIXES`
    /// (comma-separated, trimmed, empties dropped).
    pub worker_allowed_target_suffixes: Vec<String>,
    /// **Opt-in transparent header relay.** Header names whose *value* is copied
    /// verbatim from the a-leg INVITE onto every originated b-leg INVITE (the
    /// normal callee leg AND the REFER transfer-target leg — both funnel through
    /// the single `build_b_leg` mint point). Names are matched case-insensitively.
    /// **Empty = no relay**, which is the production default and a strict no-op.
    /// Structural headers the generator owns (Via/From/To/Contact/Call-ID/CSeq/
    /// Max-Forwards/Route/Record-Route/Content-Length/Content-Type) are NEVER
    /// relayable even if named here, so a misconfiguration cannot corrupt the
    /// dialog. Overridable per worker via `B2BUA_RELAY_HEADERS` (comma-separated).
    pub relay_headers: Vec<String>,
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
            // Loop/runaway guard: terminate a call after this many in-dialog
            // events. TS default is 100 (MAX_MESSAGES_PER_CALL); kept a touch
            // higher (200) so a legitimate long-hold call — keepalive OPTIONS at
            // a 300 s cadence over a multi-hour hold (~2 events/cycle) — stays
            // well under it, while a flood/glare loop is still capped. The Rust
            // port previously set 5_000 AND never enforced it, so a call grew its
            // txn/clone/store churn unbounded (the no-chaos RSS climb). Override
            // with `B2BUA_MAX_MESSAGES_PER_CALL`.
            max_messages_per_call: 200,
            cdr_buffer_queue_max: 1_024,
            refer_subscription_expiry_sec: 60,
            refer_reinvite_answer_sec: 32,
            refer_overall_safety_sec: 120,
            keepalive_interval_sec: 300,
            keepalive_timeout_sec: 32,
            reboot_budget_sec: 600,
            limiter_refresh_sec: 300,
            keepalive_catchup_speedup: 10,
            max_catchup_window_sec: None,
            reaper_enabled: true,
            reaper_sweep_interval_sec: 30,
            reaper_idle_max_sec: 0,
            setup_timeout_sec: 150,
            ack_timeout_sec: 32,
            // Tier-3 admission gate (migration/09). TS defaults
            // (CPS_BUCKET_SIZE / CPS_BUCKET_RATE / OVERLOAD_PANIC_ELU_THRESHOLD /
            // RETRY_AFTER_BASE_SEC). The hard CPS ceiling is 1000-burst @ 500/s;
            // the panic-ELU backstop sits at 0.75 EWMA-ELU.
            cps_bucket_size: 1000,
            cps_bucket_rate: 500,
            overload_panic_elu_threshold: 0.75,
            retry_after_base_sec: 5,
            // b-leg admission allow-list. TS env default is the single K8s
            // in-cluster DNS suffix `.svc.cluster.local`; production traffic
            // (pod FQDNs) always passes, bogus hostnames are 503'd pre-leg. The
            // paused-clock test harness builds configs directly; a fixture that
            // routes to a non-suffixed host (e.g. `bob` / a loopback name) must
            // set `["*"]` or add its suffix to opt out of the gate.
            worker_allowed_target_suffixes: vec![".svc.cluster.local".to_string()],
            // Opt-in transparent header relay: empty = no relay (production
            // default, strict no-op). Set names via `B2BUA_RELAY_HEADERS`.
            relay_headers: Vec::new(),
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
        // The reaper's staleness verdict is only provable when a healthy call is
        // guaranteed at least one liveness-bearing event (its keepalive OPTIONS
        // `200` — only real SIP traffic stamps the ledger) inside the idle
        // window — below 2 keepalive intervals a merely-quiet call could be
        // reaped (ADR-0020 X4).
        if self.reaper_idle_max_sec != 0
            && self.reaper_idle_max_sec < 2 * self.keepalive_interval_sec
        {
            return Err(format!(
                "reaper_idle_max_sec={} < 2 × keepalive_interval_sec={}: a healthy \
                 call is only provably dead after missing at least two keepalive \
                 cycles (0 derives 3× automatically)",
                self.reaper_idle_max_sec, self.keepalive_interval_sec
            ));
        }
        // ── Worker overload protection (Tier-3 CPS bucket + panic-ELU backstop) ──
        // The panic-ELU backstop 503s a new INVITE when the worker's own
        // EWMA-smoothed Event-Loop Utilization exceeds this fraction. ELU is
        // clamped to [0, 1], so a threshold <= 0 (or NaN) would trip for *every*
        // call the instant load is non-zero — the worker could never admit an
        // INVITE. (>= 1.0 is the documented "disable" and is allowed: the clamped
        // ELU never exceeds 1.)
        if !(self.overload_panic_elu_threshold.is_finite() && self.overload_panic_elu_threshold > 0.0)
        {
            return Err(format!(
                "overload_panic_elu_threshold={} is not a positive fraction: ELU is \
                 clamped to [0,1], so a value <= 0 (or NaN) would 503 every new \
                 INVITE the instant load rises. Use a value in (0, 1] (>= 1.0 \
                 disables the backstop)",
                self.overload_panic_elu_threshold
            ));
        }
        // The Tier-3 CPS bucket refills at `cps_bucket_rate` tokens/s up to
        // `cps_bucket_size`. If the gate is enabled (size > 0) but the rate is 0,
        // the bucket drains once and never refills — after the first burst EVERY
        // new INVITE is 503'd forever. (size == 0 disables the hard CPS gate, so
        // the rate is moot.)
        if self.cps_bucket_size > 0 && self.cps_bucket_rate == 0 {
            return Err(format!(
                "cps_bucket_rate=0 with cps_bucket_size={} (CPS gate enabled): the \
                 token bucket would drain once and never refill, 503-ing every new \
                 INVITE after the first burst. Set a positive refill rate, or set \
                 cps_bucket_size=0 to disable the hard CPS gate",
                self.cps_bucket_size
            ));
        }
        Ok(())
    }

    /// The effective reaper idle threshold, ms (ADR-0020 X4): the explicit
    /// override, or the derived `3 × keepalive_interval_sec`.
    pub fn reaper_idle_max_ms(&self) -> i64 {
        let sec = if self.reaper_idle_max_sec > 0 {
            self.reaper_idle_max_sec
        } else {
            3 * self.keepalive_interval_sec
        };
        sec.saturating_mul(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        assert!(B2buaConfig::default().validate().is_ok());
    }

    #[test]
    fn rejects_nonpositive_or_nan_panic_elu() {
        for bad in [0.0, -0.5, f64::NAN, f64::INFINITY] {
            let c = B2buaConfig {
                overload_panic_elu_threshold: bad,
                ..Default::default()
            };
            let e = c
                .validate()
                .expect_err("non-positive/NaN panic-ELU must be rejected");
            assert!(e.contains("overload_panic_elu_threshold"), "msg was: {e}");
        }
    }

    #[test]
    fn allows_panic_elu_at_or_above_one_as_disable() {
        // `>= 1.0` is the documented way to disable the backstop (clamped ELU
        // never exceeds 1) — must NOT be rejected.
        for ok in [1.0, 1.5] {
            let c = B2buaConfig {
                overload_panic_elu_threshold: ok,
                ..Default::default()
            };
            assert!(c.validate().is_ok(), "{ok} should validate");
        }
    }

    #[test]
    fn rejects_zero_cps_rate_when_gate_enabled() {
        let c = B2buaConfig {
            cps_bucket_size: 1000,
            cps_bucket_rate: 0,
            ..Default::default()
        };
        let e = c.validate().expect_err("zero refill with gate on must be rejected");
        assert!(e.contains("cps_bucket_rate"), "msg was: {e}");
    }

    #[test]
    fn allows_zero_cps_rate_when_gate_disabled() {
        // size == 0 disables the hard CPS gate, so a 0 refill rate is moot.
        let c = B2buaConfig {
            cps_bucket_size: 0,
            cps_bucket_rate: 0,
            ..Default::default()
        };
        assert!(c.validate().is_ok());
    }
}
