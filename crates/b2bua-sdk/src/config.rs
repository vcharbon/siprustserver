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
    /// Auto-terminate a call whose in-dialog event count exceeds this WITHIN
    /// one keepalive interval (loop guard). The keepalive tick resets the
    /// counter, so the budget is a rate: a runaway dialog lands >cap events
    /// inside one window and is torn down, while a healthy call outlasting any
    /// number of intervals never consumes the defense.
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
    /// Overall safety timer covering the whole **established-call reroute**
    /// (a `Route`-shaped `call_release` decision): replacement
    /// b-leg dial + a-leg re-INVITE realign + old-leg BYE. Armed when the
    /// reroute is applied, cancelled on completion; on expiry the call is torn
    /// down (the release event stands — a wedged reroute must never extend the
    /// call past its cap). Default 120, mirroring `refer_overall_safety_sec`
    /// (the same two-re-INVITE realign shape).
    pub release_reroute_guard_sec: i64,
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
    /// provably means a SIP-dead call, never quietness. The derived window is
    /// additionally floored above
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) — a ringing
    /// call stamps nothing until its setup resolves. An explicit value must be
    /// ≥ `2 × keepalive_interval_sec` AND > the transaction bound (enforced by
    /// [`validate`](Self::validate)). Liveness derives ONLY from the stamp —
    /// never `created_at`, never timer deadlines.
    pub reaper_idle_max_sec: i64,
    /// **Setup timeout**, seconds — the call-level a-leg initial-INVITE
    /// deadline: armed at route time, cancelled at answer, deliberately NOT
    /// reset by reroute/failover (each new b-leg gets its own `NoAnswer`; this
    /// caps the caller's *total* wait for a final response). It rides the
    /// replicated `call.timers` ledger, so it survives a crash → reclaim —
    /// the sip-txn transaction bound
    /// ([`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec)) cannot (the
    /// transactions die with the node), which is how a worker kill stranded
    /// mid-setup calls holding limiter slots for the full 1 h GlobalDuration
    /// (endurance 2026-06-12). Default **150 s**: strictly below the
    /// configured transaction bound (enforced by [`validate`](Self::validate))
    /// so the rules path owns the teardown (408 to the caller, CANCEL to
    /// pending b-legs, obligations settled), above any sane no-answer timeout
    /// so a route-supplied `NoAnswer` still fires first. `<= 0` disables (the
    /// txn backstop and GlobalDuration remain). Overridable via
    /// `B2BUA_SETUP_TIMEOUT_SEC`.
    pub setup_timeout_sec: i64,
    /// **Initial-INVITE transaction bound**, seconds — the sip-txn
    /// out-of-dialog INVITE give-up window
    /// (`TransactionConfig::invite_initial_timeout_ms`), bounding BOTH halves
    /// of a call: the b-leg client txn's give-up AND the a-leg server txn's
    /// pre-final sweep age derive from it. The last-resort backstop under every
    /// app-level setup deadline: [`validate`](Self::validate) requires
    /// `setup_timeout_sec < invite_txn_timeout_sec` (when enabled) and
    /// range-checks [`MIN_INVITE_TXN_TIMEOUT_SEC`](Self::MIN_INVITE_TXN_TIMEOUT_SEC)
    /// `..=` [`MAX_INVITE_TXN_TIMEOUT_SEC`](Self::MAX_INVITE_TXN_TIMEOUT_SEC);
    /// a route-supplied `NoAnswer` above
    /// `bound − `[`NO_ANSWER_CANCEL_MARGIN_SEC`](Self::NO_ANSWER_CANCEL_MARGIN_SEC)
    /// is clamped to that ceiling.
    /// Default **158 s**; telephony deployments (Timer C > 3 min, 180 s PSTN
    /// supervision, hunting chains) raise it. Overridable via
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC`.
    pub invite_txn_timeout_sec: i64,
    /// **Initial-INVITE first-response bound**, seconds — how long a b-leg
    /// initial INVITE waits for a response of ANY kind (not even a `100
    /// Trying`) before its client transaction gives up with a
    /// `TimeoutKind::Response` and the `call_failure` consult carries
    /// `timeout_kind: "response"` — the "is this hop alive?" question, distinct
    /// from [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec)'s "how
    /// long may it ring?": the first provisional swaps the long bound in. A
    /// **deliberate RFC 3261 §17.1.1.2 deviation, telephony policy**: the RFC's
    /// Timer B is 64·T1 = 32 s, and a caller must not hear silence for half a
    /// minute before the reroute to an alternate hop. Only the initial INVITE
    /// reads it — an in-dialog INVITE keeps Timer B and a non-INVITE Timer F.
    /// The Timer A ladder is cut at the bound, so the bound buys a rung count:
    /// 2 s → 2 re-sends (0.5/1.5), 5 s → 3 (0.5/1.5/3.5), 10 s → 4, 32 s → 6.
    /// [`validate`](Self::validate) range-checks
    /// [`MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`](Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC)
    /// `..=` [`MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`](Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC).
    /// Default **32 s** (Timer B: no deviation until a deployment asks).
    /// Overridable via
    /// `B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`.
    pub invite_first_response_timeout_sec: i64,
    /// **Strict RFC 3261 §9.1 CANCEL wait** (ADR-0028). `false` (the default)
    /// is the bounded-hold policy: a b-leg CANCEL for a response-less branch
    /// waits for the first provisional at most the sip-txn grace window
    /// (`CANCEL_HOLD_GRACE`, 1 s), then goes on the wire regardless — every
    /// emitted CANCEL reaches the callee, so an abandoned setup can never ring
    /// to the callee's own give-up (and behind the 100-absorbing LB, a
    /// 100-only b-leg is still CANCELed). `true` is the literal §9.1 wait: no
    /// CANCEL is ever sent pre-provisional — a callee that answers nothing is
    /// never CANCELed and rides the terminating backstop instead. Overridable
    /// via `B2BUA_CANCEL_STRICT_RFC_WAIT` (truthy = strict).
    pub cancel_strict_rfc3261_wait: bool,
    /// **Un-ACKed 2xx give-up deadline**, seconds (RFC 3261 §13.3.1.4; the
    /// RFC's `64·T1` = 32 s is the default). When it elapses with no ACK for a
    /// 2xx to an INVITE — initial or re-INVITE — the peer is gone and the
    /// session ends: the B2BUA BYEs the a-leg dialog and the b-leg it bridges.
    /// Only the deadline is configured: the 2xx ladder always runs to Timer L
    /// and the teardown always acts (ADR-0032 X5). `<= 0` is NOT allowed —
    /// [`validate`](Self::validate) refuses it, and a harness config that
    /// writes it anyway gets the default
    /// ([`ack_timeout_ms`](Self::ack_timeout_ms)), as
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) does.
    /// Overridable via `B2BUA_ACK_TIMEOUT_SEC`.
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
    /// **Decision-backend deadline** (ms) — the hard per-round-trip bound on a
    /// caller-blocking `CallDecisionEngine` call (`new_call` and `call_failure`;
    /// `call_refer` is bounded by the refer subscription/safety timers instead —
    /// see [`crate::…`] `DeadlineDecisionEngine`), enforced by the CORE
    /// (`b2bua::decision::DeadlineDecisionEngine` wraps whatever engine the host
    /// injects at `B2buaCore::spawn`). This is the load-bearing half of the
    /// initial-INVITE guarantee (ADR-0022): sip-txn auto-answers 100 Trying
    /// before the router ever sees the INVITE, so the caller is already waiting
    /// — a decision backend that hangs must NOT strand them. On expiry the call
    /// takes the ordinary decision-error path (503 to the caller for
    /// `new_call`; the per-site `Err` fallback for failure/refer). The TS
    /// system enforced this INSIDE its `HttpReferenceAdapter`
    /// (`callControlNewCallTimeoutMs`/`callControlFailureTimeoutMs`, both
    /// default 5000); the Rust port moves it into the core as ONE knob so a
    /// third-party adapter cannot forget it. Default **5000**. `<= 0` disables
    /// (escape hatch for tests that need a genuinely wedged decision await —
    /// the reaper ladder still cleans up, see `reaper.rs`). Overridable via
    /// `B2BUA_CALL_CONTROL_TIMEOUT_MS`.
    pub call_control_timeout_ms: i64,
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
    /// **Default SDP source** (a service parameter). A canned SDP body a service
    /// rule can source (via `ctx.config`) to originate a deliberate *fake-offer*
    /// INVITE — e.g. a delayed-offer flow where the service sends `INVITE(SDP)` it
    /// authored itself. A `CreateLeg` opts in explicitly with
    /// `body_override: ctx.config.default_sdp.clone()`; it is **never** an
    /// automatic fallback (a normal reroute/failover `CreateLeg` passes
    /// `body_override: None` on purpose to relay the caller's own offer). `None`
    /// (the default) = no canned SDP, today's behaviour.
    pub default_sdp: Option<Vec<u8>>,
    /// **Node capability advertisement.** The `Allow`/`Supported` set this
    /// worker advertises on the out-of-dialog OPTIONS health reply (RFC 3261
    /// §11.2) — a node-scoped statement, not a call-scoped one, so it is
    /// declared here rather than by a routing decision. The default is the
    /// stack set; the health path borrows this value, so a keepalive probe
    /// resolves its advertisement without allocating.
    pub node_capabilities: sip_message::generators::CapabilitySet,
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
            // Loop/runaway guard, per keepalive interval (the tick resets the
            // counter — see the field doc). TS default is 100
            // (MAX_MESSAGES_PER_CALL); kept a touch higher (200) so any
            // legitimate per-interval burst stays well under it while a
            // flood/glare loop is still capped inside one window. Override with
            // `B2BUA_MAX_MESSAGES_PER_CALL`.
            max_messages_per_call: 200,
            cdr_buffer_queue_max: 1_024,
            refer_subscription_expiry_sec: 60,
            refer_reinvite_answer_sec: 32,
            refer_overall_safety_sec: 120,
            release_reroute_guard_sec: 120,
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
            invite_txn_timeout_sec: 158,
            invite_first_response_timeout_sec: Self::DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC,
            cancel_strict_rfc3261_wait: false,
            ack_timeout_sec: Self::DEFAULT_ACK_TIMEOUT_SEC,
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
            // Decision-backend deadline: TS CALL_CONTROL_*_TIMEOUT_MS parity
            // (both were 5000). One knob for all three methods (ADR-0022).
            call_control_timeout_ms: 5_000,
            // Opt-in transparent header relay: empty = no relay (production
            // default, strict no-op). Set names via `B2BUA_RELAY_HEADERS`.
            relay_headers: Vec::new(),
            // Default SDP source: None = no canned offer. Opt-in per-CreateLeg
            // via `body_override`, never an automatic fallback.
            default_sdp: None,
            node_capabilities: sip_message::generators::CapabilitySet::default(),
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
    /// Floor for [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) (s):
    /// the out-of-dialog bound must stay above the 32 s in-dialog Timer B.
    pub const MIN_INVITE_TXN_TIMEOUT_SEC: i64 = 33;
    /// Ceiling for [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) (s):
    /// the top of the supported telephony setup range (~10 min hunting chains).
    pub const MAX_INVITE_TXN_TIMEOUT_SEC: i64 = 600;
    /// Floor for
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): two Timer A re-sends (rungs at 0.5 s and 1.5 s) precede the
    /// give-up, so a lossy path still gets more than one chance.
    pub const MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 2;
    /// Ceiling for
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): Timer B itself (RFC 3261 §17.1.1.2, 64·T1) — nothing above the RFC
    /// value is expressible.
    pub const MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 32;
    /// Default
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): Timer B, the RFC value.
    pub const DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 32;
    /// Margin (s) a route-supplied `NoAnswer` deadline must keep under
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec): the room the
    /// CANCEL→487 exchange needs to complete inside the still-live b-leg client
    /// transaction (mirrors the default 150/158 gap).
    pub const NO_ANSWER_CANCEL_MARGIN_SEC: i64 = 8;
    /// Default [`ack_timeout_sec`](Self::ack_timeout_sec) (s): RFC 3261
    /// §13.3.1.4's `64·T1`, Timer L.
    pub const DEFAULT_ACK_TIMEOUT_SEC: i64 = 32;

    /// Validate operator-supplied tunables at **boot** (the runner calls this and
    /// refuses to start on `Err`; unit/sim harnesses construct configs directly
    /// and skip it). Returns the first violation as a human-readable message.
    pub fn validate(&self) -> Result<(), String> {
        if self.keepalive_interval_sec < Self::MIN_KEEPALIVE_SEC {
            return Err(format!(
                "keepalive_interval_sec={} < min {} s (2 min): a shorter in-dialog \
                 OPTIONS cadence breaks long-hold traffic",
                self.keepalive_interval_sec,
                Self::MIN_KEEPALIVE_SEC
            ));
        }
        if self.reboot_budget_sec < Self::MIN_REBOOT_BUDGET_SEC {
            return Err(format!(
                "reboot_budget_sec={} < min {} s (1 min): a replicated backup must \
                 survive a primary reboot",
                self.reboot_budget_sec,
                Self::MIN_REBOOT_BUDGET_SEC
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
        if !(self.overload_panic_elu_threshold.is_finite()
            && self.overload_panic_elu_threshold > 0.0)
        {
            return Err(format!(
                "overload_panic_elu_threshold={} is not a positive fraction: ELU is \
                 clamped to [0,1], so a value <= 0 (or NaN) would 503 every new \
                 INVITE the instant load rises. Use a value in (0, 1] (>= 1.0 \
                 disables the backstop)",
                self.overload_panic_elu_threshold
            ));
        }
        // The transaction bound must sit in the supported range: above the 32 s
        // in-dialog Timer B (the floor keeps the out-of-dialog window the LONG
        // one), at most the telephony ceiling.
        if !(Self::MIN_INVITE_TXN_TIMEOUT_SEC..=Self::MAX_INVITE_TXN_TIMEOUT_SEC)
            .contains(&self.invite_txn_timeout_sec)
        {
            return Err(format!(
                "invite_txn_timeout_sec={} outside the supported {}..={} s range",
                self.invite_txn_timeout_sec,
                Self::MIN_INVITE_TXN_TIMEOUT_SEC,
                Self::MAX_INVITE_TXN_TIMEOUT_SEC
            ));
        }
        // The first-response bound is a deadline between the two-rung floor
        // and Timer B: below the floor the INVITE is effectively sent once,
        // above Timer B the RFC value already owns the give-up.
        if !(Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ..=Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC)
            .contains(&self.invite_first_response_timeout_sec)
        {
            return Err(format!(
                "invite_first_response_timeout_sec={} outside the supported {}..={} s range",
                self.invite_first_response_timeout_sec,
                Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC,
                Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ));
        }
        // Above the transaction bound the ordering inverts: the txn layer
        // CANCELs the b-leg on its own, the app give-up authors a SECOND final,
        // and the callee's 487 lands on a dead client txn (never ACKed). The
        // app setup deadline must therefore fire strictly first.
        if self.setup_timeout_sec > 0 && self.setup_timeout_sec >= self.invite_txn_timeout_sec {
            return Err(format!(
                "setup_timeout_sec={} >= invite_txn_timeout_sec={}: the app setup \
                 deadline must fire strictly before the transaction bound, or the \
                 txn layer tears the b-leg down first and the caller takes two \
                 finals on one INVITE",
                self.setup_timeout_sec, self.invite_txn_timeout_sec
            ));
        }
        // A ringing call stamps no ledger liveness until its setup resolves, so
        // an explicit reaper idle window at or under the transaction bound would
        // reap a call that is legitimately still ringing (the derived window is
        // floored above the bound automatically — see `reaper_idle_max_ms`).
        if self.reaper_idle_max_sec != 0 && self.reaper_idle_max_sec <= self.invite_txn_timeout_sec
        {
            return Err(format!(
                "reaper_idle_max_sec={} <= invite_txn_timeout_sec={}: the reaper idle \
                 window must outlast the longest configured ring, or a legitimately \
                 still-ringing call is reaped before its own setup deadline",
                self.reaper_idle_max_sec, self.invite_txn_timeout_sec
            ));
        }
        // The un-ACKed 2xx give-up is a deadline, never a switch (RFC 3261
        // §13.3.1.4): a non-positive value would configure a leaking session.
        if self.ack_timeout_sec <= 0 {
            return Err(format!(
                "ack_timeout_sec={} is not a positive deadline: an un-ACKed 2xx \
                 always ends the session (RFC 3261 §13.3.1.4), the knob only says \
                 how many seconds to wait first (default {})",
                self.ack_timeout_sec,
                Self::DEFAULT_ACK_TIMEOUT_SEC
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

    /// The configured initial-INVITE transaction bound in ms — the value wired
    /// into `TransactionConfig::invite_initial_timeout_ms`. `<= 0` is NOT
    /// "disabled" (unlike [`setup_timeout_sec`](Self::setup_timeout_sec)):
    /// a non-positive value falls back to the 158 s default rather than arming
    /// a degenerate bound.
    pub fn invite_txn_timeout_ms(&self) -> u64 {
        let sec = if self.invite_txn_timeout_sec > 0 { self.invite_txn_timeout_sec } else { 158 };
        u64::try_from(sec).unwrap_or(158).saturating_mul(1000)
    }

    /// The configured initial-INVITE first-response bound in ms — the value
    /// wired into `TransactionConfig::invite_first_response_timeout_ms`. `<= 0`
    /// is NOT "disabled": a non-positive value falls back to the 32 s Timer B
    /// default rather than arming a degenerate bound.
    pub fn invite_first_response_timeout_ms(&self) -> u64 {
        let default = Self::DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC;
        let sec = if self.invite_first_response_timeout_sec > 0 {
            self.invite_first_response_timeout_sec
        } else {
            default
        };
        u64::try_from(sec).unwrap_or(default as u64).saturating_mul(1000)
    }

    /// The un-ACKed 2xx give-up deadline in ms — what arms a 2xx ladder's
    /// give-up. `<= 0` is NOT "disabled" (unlike
    /// [`setup_timeout_sec`](Self::setup_timeout_sec)): a non-positive value
    /// falls back to the 32 s default rather than leaving a 2xx's silence
    /// unanswered (RFC 3261 §13.3.1.4).
    pub fn ack_timeout_ms(&self) -> u64 {
        let sec = if self.ack_timeout_sec > 0 {
            self.ack_timeout_sec
        } else {
            Self::DEFAULT_ACK_TIMEOUT_SEC
        };
        u64::try_from(sec).unwrap_or(Self::DEFAULT_ACK_TIMEOUT_SEC as u64).saturating_mul(1000)
    }

    /// Clamp a route-supplied `NoAnswer` deadline (s) under the transaction
    /// bound: any value above `bound − `[`NO_ANSWER_CANCEL_MARGIN_SEC`](Self::NO_ANSWER_CANCEL_MARGIN_SEC)
    /// — including one merely inside the margin band, which would equally
    /// strand the CANCEL→487 exchange — is held to that ceiling so the
    /// exchange completes inside the live b-leg client transaction. Values at
    /// or under the ceiling pass through unchanged; the caller compares the
    /// result to detect (and `debug!`-note) a clamp.
    pub fn clamp_no_answer_sec(&self, requested: i64) -> i64 {
        let ceiling = self.invite_txn_timeout_sec - Self::NO_ANSWER_CANCEL_MARGIN_SEC;
        requested.min(ceiling)
    }

    /// The effective reaper idle threshold, ms (ADR-0020 X4): the explicit
    /// override, or the derived `3 × keepalive_interval_sec` floored above
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) — a
    /// legitimately-ringing call stamps no ledger liveness until its setup
    /// resolves, so the idle window must outlast the longest configured ring.
    pub fn reaper_idle_max_ms(&self) -> i64 {
        let sec = if self.reaper_idle_max_sec > 0 {
            self.reaper_idle_max_sec
        } else {
            (3 * self.keepalive_interval_sec)
                .max(self.invite_txn_timeout_sec + self.keepalive_interval_sec)
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
            let c = B2buaConfig { overload_panic_elu_threshold: bad, ..Default::default() };
            let e = c.validate().expect_err("non-positive/NaN panic-ELU must be rejected");
            assert!(e.contains("overload_panic_elu_threshold"), "msg was: {e}");
        }
    }

    #[test]
    fn allows_panic_elu_at_or_above_one_as_disable() {
        // `>= 1.0` is the documented way to disable the backstop (clamped ELU
        // never exceeds 1) — must NOT be rejected.
        for ok in [1.0, 1.5] {
            let c = B2buaConfig { overload_panic_elu_threshold: ok, ..Default::default() };
            assert!(c.validate().is_ok(), "{ok} should validate");
        }
    }

    #[test]
    fn rejects_zero_cps_rate_when_gate_enabled() {
        let c = B2buaConfig { cps_bucket_size: 1000, cps_bucket_rate: 0, ..Default::default() };
        let e = c.validate().expect_err("zero refill with gate on must be rejected");
        assert!(e.contains("cps_bucket_rate"), "msg was: {e}");
    }

    #[test]
    fn allows_zero_cps_rate_when_gate_disabled() {
        // size == 0 disables the hard CPS gate, so a 0 refill rate is moot.
        let c = B2buaConfig { cps_bucket_size: 0, cps_bucket_rate: 0, ..Default::default() };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_setup_deadline_at_or_above_the_txn_bound() {
        // 300 >= 158: the txn layer would CANCEL the b-leg before the app
        // gives up (the ordering inversion) — refuse to start.
        let c = B2buaConfig {
            setup_timeout_sec: 300,
            invite_txn_timeout_sec: 158,
            ..Default::default()
        };
        let e = c.validate().expect_err("setup >= txn bound must be rejected");
        assert!(e.contains("setup_timeout_sec"), "msg was: {e}");
    }

    #[test]
    fn rejects_txn_bound_outside_the_supported_range() {
        for bad in [700, 20] {
            let c = B2buaConfig {
                invite_txn_timeout_sec: bad,
                setup_timeout_sec: 0,
                ..Default::default()
            };
            let e = c.validate().expect_err("out-of-range invite_txn_timeout_sec must be rejected");
            assert!(e.contains("invite_txn_timeout_sec"), "msg was: {e}");
        }
    }

    #[test]
    fn first_response_bound_is_a_deadline_between_two_rungs_and_timer_b() {
        // Default is Timer B itself: no deviation until a deployment asks for
        // one.
        assert_eq!(B2buaConfig::default().invite_first_response_timeout_sec, 32);
        assert_eq!(B2buaConfig::default().invite_first_response_timeout_ms(), 32_000);
        // 1 s would send the INVITE effectively once; 33 s is above the RFC
        // value the class already gives up at.
        for bad in [1, 33, 0, -5] {
            let c = B2buaConfig { invite_first_response_timeout_sec: bad, ..Default::default() };
            let e = c
                .validate()
                .expect_err("out-of-range invite_first_response_timeout_sec must be rejected");
            assert!(e.contains("invite_first_response_timeout_sec"), "msg was: {e}");
        }
        // The floor (two re-sends) and the ceiling (Timer B) are both allowed.
        for ok in [2, 5, 32] {
            let c = B2buaConfig { invite_first_response_timeout_sec: ok, ..Default::default() };
            assert!(c.validate().is_ok(), "{ok} s must be accepted");
            assert_eq!(c.invite_first_response_timeout_ms(), ok as u64 * 1000);
        }
        // A harness config that writes a non-positive value anyway arms the
        // Timer B default, never a degenerate bound.
        let c = B2buaConfig { invite_first_response_timeout_sec: 0, ..Default::default() };
        assert_eq!(c.invite_first_response_timeout_ms(), 32_000);
    }

    #[test]
    fn allows_setup_deadline_strictly_below_a_raised_txn_bound() {
        let c = B2buaConfig {
            setup_timeout_sec: 300,
            invite_txn_timeout_sec: 400,
            ..Default::default()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn disabled_setup_deadline_skips_the_ordering_check() {
        // <= 0 disables the app deadline; only the range check applies.
        let c =
            B2buaConfig { setup_timeout_sec: 0, invite_txn_timeout_sec: 158, ..Default::default() };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn clamps_no_answer_above_the_margin_ceiling() {
        let c = B2buaConfig { invite_txn_timeout_sec: 200, ..Default::default() };
        // Above `bound − margin` → held to the ceiling; at/under it → untouched.
        assert_eq!(c.clamp_no_answer_sec(250), 192);
        assert_eq!(c.clamp_no_answer_sec(200), 192);
        // Inside the margin band (192 < 195 < 200): equally clamped — a value
        // there would strand the CANCEL→487 exchange just the same.
        assert_eq!(c.clamp_no_answer_sec(195), 192);
        assert_eq!(c.clamp_no_answer_sec(192), 192);
        assert_eq!(c.clamp_no_answer_sec(191), 191);
        assert_eq!(c.clamp_no_answer_sec(30), 30);
    }

    #[test]
    fn derived_reaper_idle_window_is_floored_above_the_txn_bound() {
        // 3 × keepalive (360 s) would undercut a 400 s ring: the derived window
        // must floor at bound + one keepalive interval.
        let c = B2buaConfig {
            invite_txn_timeout_sec: 400,
            keepalive_interval_sec: 120,
            reaper_idle_max_sec: 0,
            ..Default::default()
        };
        assert_eq!(c.reaper_idle_max_ms(), 520_000);
        // Default bound (158 s): the 3× derivation already clears it — unchanged.
        let d = B2buaConfig::default();
        assert_eq!(d.reaper_idle_max_ms(), 3 * d.keepalive_interval_sec * 1000);
    }

    #[test]
    fn rejects_explicit_reaper_idle_window_at_or_under_the_txn_bound() {
        // keepalive 150 keeps the 2×-keepalive rule satisfied (400 ≥ 300), so
        // the txn-bound rule is what rejects the 400 s window against a 400 s
        // ring.
        let c = B2buaConfig {
            invite_txn_timeout_sec: 400,
            setup_timeout_sec: 300,
            keepalive_interval_sec: 150,
            reaper_idle_max_sec: 400,
            ..Default::default()
        };
        let e =
            c.validate().expect_err("an idle window a ringing call can outlive must be rejected");
        assert!(e.contains("invite_txn_timeout_sec"), "msg was: {e}");
        let ok = B2buaConfig { reaper_idle_max_sec: 401, ..c };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn rejects_a_nonpositive_ack_deadline() {
        // RFC 3261 §13.3.1.4: the give-up is a deadline, not a switch — no
        // value may configure an un-ACKed 2xx that never ends the session.
        for bad in [0, -5] {
            let c = B2buaConfig { ack_timeout_sec: bad, ..Default::default() };
            let e = c.validate().expect_err("a non-positive ACK deadline must be rejected");
            assert!(e.contains("ack_timeout_sec"), "msg was: {e}");
        }
        assert!(B2buaConfig { ack_timeout_sec: 1, ..Default::default() }.validate().is_ok());
    }

    #[test]
    fn nonpositive_ack_deadline_falls_back_to_the_default_ms() {
        // A harness config that writes 0 anyway arms the 32 s default, never a
        // ladder whose give-up tears nothing down.
        for bad in [0, -5] {
            let c = B2buaConfig { ack_timeout_sec: bad, ..Default::default() };
            assert_eq!(c.ack_timeout_ms(), 32_000);
        }
        assert_eq!(
            B2buaConfig { ack_timeout_sec: 6, ..Default::default() }.ack_timeout_ms(),
            6_000
        );
    }

    #[test]
    fn nonpositive_txn_bound_falls_back_to_the_default_ms() {
        // 0 is NOT "disabled" for the transaction bound (validate refuses it at
        // boot); a harness config that writes it anyway gets the 158 s default,
        // never a degenerate near-zero bound.
        let c = B2buaConfig { invite_txn_timeout_sec: 0, ..Default::default() };
        assert_eq!(c.invite_txn_timeout_ms(), 158_000);
    }
}
