//! [`RunnerEnv`] — the generic runner knobs and their env grammar, read once.
//! A value the worker cannot honour refuses the boot, naming its variable: a
//! typo'd knob never silently becomes a default.

use std::env;
use std::fmt::Display;
use std::str::FromStr;
use std::time::Duration;

use b2bua::config::CapacityConfig;

use crate::{
    capacity_env, drain_env, env_or, limiter_env, liveness_env, parse_outbound_proxy, split_csv,
    stated, stated_list,
};

/// Every generic runner knob, parsed once from env by [`RunnerEnv::from_env`].
/// All fields are pub: tweak any of them between `from_env()` and
/// [`bind`](RunnerEnv::bind) to override a single knob without re-implementing
/// the rest (a hardcoded listen port, a test-only drain grace, …).
pub struct RunnerEnv {
    /// `B2BUA_LISTEN` — SIP/signaling listen addr (default `0.0.0.0:5060`).
    pub listen: String,
    /// `B2BUA_ADVERTISE` — SIP `host[:port]` stamped on Via/Contact/b-leg
    /// Call-ID. `None` → the bound IP, with an unspecified `0.0.0.0` coerced to
    /// loopback so the literal is at least routable. In k8s inject the pod IP
    /// via the downward API `status.podIP`, else peers route responses to
    /// `0.0.0.0` (a storm).
    pub advertise: Option<String>,
    /// `B2BUA_OUTBOUND_PROXY` — front-proxy IP-literal `ip:port` every b-leg
    /// (worker→callee) request is forced through (a preloaded plain `;lr` Route).
    /// REQUIRED in the k8s cluster (pod IPs are not peer-routable); unset →
    /// b-leg goes straight to the callee (local/dev only). A malformed value is
    /// fatal at parse — a silent fallback to pod-direct would send b-leg traffic
    /// to unroutable pod IPs.
    pub outbound_proxy: Option<(String, u16)>,
    /// `B2BUA_REQUIRE_OUTBOUND_PROXY` — set (to ANY non-empty value) to refuse
    /// boot when `B2BUA_OUTBOUND_PROXY` is unset. In the cluster profile a
    /// worker silently dialing R-URIs pod-direct violates the "only the proxy
    /// sees the external world" invariant (the dual-face network split);
    /// refusing the boot turns that misconfiguration into a loud
    /// CrashLoopBackOff instead of a live plane leak.
    pub require_outbound_proxy: bool,
    /// `B2BUA_METRICS` — Prometheus HTTP listen addr (default `0.0.0.0:9091`).
    pub metrics_addr: String,
    /// `B2BUA_QUEUE` — inbound UDP queue depth, packets (default 8192).
    pub queue_max: usize,
    /// `B2BUA_UDP_SNDBUF` — `SO_SNDBUF` requested on the signalling socket,
    /// bytes (default 4 MiB; empty = the kernel's `wmem_default`; clamped at
    /// `wmem_max`). See ADR-0033.
    pub udp_sndbuf: Option<usize>,
    /// `B2BUA_UDP_RCVBUF` — `SO_RCVBUF` requested on the signalling socket,
    /// bytes (default 4 MiB; empty = the kernel's `rmem_default`; clamped at
    /// `rmem_max`). What overflows it is dropped by the kernel and exported as
    /// `b2bua_udp_kernel_rx_dropped_total`.
    pub udp_rcvbuf: Option<usize>,
    /// `B2BUA_CDR_QUEUE` — buffered-CDR submit queue depth (default 1024).
    pub cdr_queue: usize,
    /// `B2BUA_ORDINAL` — worker ordinal stamped in callRef (default `w0`).
    pub ordinal: String,
    /// `POD_UID` — the worker identity, the orchestrator's pod UID from the
    /// downward API (ADR-0055 §6). Unset (a static pool): a per-boot token
    /// drawn from the OS RNG.
    pub identity: Option<String>,
    /// `B2BUA_CONCURRENCY` — handler concurrency ceiling (default 8192; a
    /// back-pressure SAFETY limit, not a rate governor).
    pub concurrency: usize,
    /// `B2BUA_CALL_CAP` — max concurrent calls before drop (default 1_000_000).
    pub call_cap: usize,
    /// `B2BUA_NEW_CALL_PERMIT_SHARE_PCT` — share of the handler concurrency
    /// normal initial-INVITE turns may hold, percent (default 50).
    pub new_call_permit_share_percent: u8,
    /// `B2BUA_NEW_CALL_QUEUE_HEADROOM_PCT` — share of the call cap kept back
    /// from normal new INVITEs, percent (default 5).
    pub new_call_queue_headroom_percent: u8,
    /// `B2BUA_MAX_MESSAGES_PER_CALL` — loop/runaway cap-defense (default 200:
    /// headroom for a multi-hour keepalive-held call).
    pub max_messages_per_call: u64,
    /// `B2BUA_MAX_MESSAGES_PER_CALL_LIFETIME` — the per-call work bound over
    /// the call's life (default 100_000; 0 is refused).
    pub max_messages_per_call_lifetime: u64,
    /// `B2BUA_KEEPALIVE_SEC` — in-dialog OPTIONS keepalive interval (default
    /// 300 s; a shorter poke breaks long-hold endurance traffic).
    pub keepalive_sec: i64,
    /// `B2BUA_KEEPALIVE_TIMEOUT_SEC` — grace waiting for the OPTIONS 200 before
    /// declaring the leg dead and BYE-ing (default 32 s so a reclaimed dialog's
    /// keepalive can round-trip across the post-reboot recovery window).
    pub keepalive_timeout_sec: i64,
    /// The replication tunables: `B2BUA_REBOOT_BUDGET_SEC`, the replica TTL /
    /// reboot budget (default 600, held past one keepalive at boot), and the
    /// backup ceilings `B2BUA_REPL_BACKUP_MAX_*` (grammar in `capacity_env`).
    pub replica: b2bua::ReplicaConfig,
    /// `B2BUA_SETUP_TIMEOUT_MS` — a-leg total setup deadline, reroutes
    /// included, in ms (default 150000; must stay strictly below
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC`, enforced at boot; <= 0 disables).
    pub setup_timeout_ms: i64,
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC` — the sip-txn out-of-dialog INVITE bound
    /// for BOTH call halves (b-leg client give-up + a-leg pre-final backstop).
    /// Default 158; supported range 33..=600 (telephony deployments raise it —
    /// Timer C > 3 min, 180 s PSTN supervision). `validate()` refuses boot when
    /// `B2BUA_SETUP_TIMEOUT_MS` does not sit strictly below it.
    pub invite_txn_timeout_sec: i64,
    /// `B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC` — how long the worker waits
    /// for a response of ANY kind to a b-leg initial INVITE before it gives up
    /// on the hop and the `call_failure` consult carries `timeout_kind:
    /// "response"` (ADR-0044). Default 32 (RFC 3261 Timer B); supported range
    /// 2..=32. In-dialog INVITE and non-INVITE never read it.
    pub invite_first_response_timeout_sec: i64,
    /// `B2BUA_CALL_CONTROL_TIMEOUT_MS` — decision-backend deadline per
    /// round-trip (default 5000; <= 0 disables — ADR-0022).
    pub call_control_timeout_ms: i64,
    /// `B2BUA_ACK_TIMEOUT_SEC` — un-ACKed 2xx give-up deadline (RFC 3261
    /// §13.3.1.4, 64·T1 = 32 s; <= 0 tears nothing down — the ladder itself
    /// always runs, ADR-0032 X5).
    pub ack_timeout_sec: i64,
    /// `B2BUA_CPS_BUCKET_SIZE` — CPS bucket rung capacity (default 1000).
    pub cps_bucket_size: u32,
    /// `B2BUA_CPS_BUCKET_RATE` — CPS bucket rung refill rate (default 500).
    pub cps_bucket_rate: u32,
    /// `B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD` — panic-ELU backstop (default 0.75).
    pub overload_panic_elu_threshold: f64,
    /// `B2BUA_RETRY_AFTER_BASE_SEC` — base Retry-After on overload 503s, floored
    /// at 1 s (default 5).
    pub retry_after_base_sec: u32,
    /// `WORKER_ALLOWED_TARGET_SUFFIXES` — b-leg destination allow-list,
    /// comma-separated (default `.svc.cluster.local`; `*` = allow all, the
    /// rollback sentinel; non-IP non-matching hosts are 503'd pre-leg).
    pub worker_allowed_target_suffixes: Vec<String>,
    /// `B2BUA_UDP_INGRESS_BRAKE_PCT` — ingress brake threshold percentage
    /// (default 70): at inbound-queue depth >= floor(queue_max × pct/100) a
    /// new, non-emergency INVITE is shed with a STATELESS 503 before the parser
    /// runs — the cheapest shed in the stack, ahead of the router's admission rungs.
    pub udp_ingress_brake_pct: u32,
    /// `B2BUA_RETRY_AFTER_JITTER_SEC` — every new-call 503's Retry-After is
    /// uniform over `[max(base, 1), max(base, 1) + jitter]` (default 5).
    pub retry_after_jitter_sec: u32,
    /// `B2BUA_RELAY_HEADERS` — opt-in transparent header relay, comma-separated
    /// names copied from the a-leg INVITE onto every originated b-leg INVITE
    /// (default empty = no relay; structural headers never relayable).
    pub relay_headers: Vec<String>,
    /// `B2BUA_PRIVACY_SERVICE` — `on` (the default): this worker is the RFC
    /// 3323 privacy service at the trust boundary; `off`: the next hop is.
    pub privacy_service: bool,
    /// `B2BUA_CDR_MESSAGE_RING` — the per-leg message-ring cap on the call
    /// record (default 0 = off; blank = unset).
    pub cdr_message_ring: usize,
    /// `B2BUA_CDR_CAPTURED_HEADERS` — the header names every ring entry
    /// captures the values of, comma-separated (default empty).
    pub cdr_captured_headers: Vec<String>,
    /// `LIMITER_URL` as the limiter's `host:port` (boot refuses a malformed
    /// value); empty when unset → `NoopLimiter` (fail-open).
    pub limiter_url: String,
    /// `LIMITER_TIMEOUT_MS` — per-request fail-open budget (default 150).
    pub limiter_timeout_ms: u64,
    /// `LIMITER_REFRESH_SECONDS` — how often a counted call extends its lease;
    /// with one refresh tick, below the lease the limiter's answers state
    /// (default 40).
    pub limiter_refresh_sec: i64,
    /// `LIMITER_REFRESH_BATCH_MS` — the worker's refresh tick: a refresh due
    /// leaves within it, with every other key due (default 1000).
    pub limiter_refresh_batch_ms: u64,
    /// `LIMITER_REFRESH_BATCH_MAX` — most keys one refresh request carries
    /// (default 1000).
    pub limiter_refresh_batch_max: usize,
    /// `LIMITER_REFRESH_TIMEOUT_MS` — the refresh request budget (default 2000).
    pub limiter_refresh_timeout_ms: u64,
    /// `LIMITER_RELEASE_TIMEOUT_MS` — the release request budget (default 2000).
    pub limiter_release_timeout_ms: u64,
    /// `LIMITER_RELEASE_QUEUE_CAP` — most releases the worker's release queue
    /// holds; the oldest is dropped past it (default 100000).
    pub limiter_release_queue_cap: usize,
    /// `LIMITER_BREAKER_*`, `LIMITER_LOOKUP_*`, `LIMITER_MAX_IN_FLIGHT`, … —
    /// the limiter client's breaker, lookups and connections (`limiter_env`,
    /// [`http_net::BackendConfig`]).
    pub limiter_backend: http_net::BackendConfig,
    /// The planned exit's bounds, the grammar in `drain_env`:
    /// `B2BUA_DRAIN_GRACE_MS` (the wait for the live calls), `B2BUA_DRAIN_MIN_MS`
    /// (the floor of a withdrawn worker's caught-up exit, ADR-0031 D2),
    /// `B2BUA_DRAIN_RELEASE_FLUSH_MS` (the wait for the queued limiter releases)
    /// and `B2BUA_DRAIN_CDR_FLUSH_MS` (the wait for the CDRs the sink holds).
    pub drain: b2bua::drain::DrainBounds,
    /// The memory ceilings (ADR-0037), every one off unless stated; the
    /// grammar is in `capacity_env`.
    pub capacity: CapacityConfig,
    /// How long one turn of the transaction owner or router may run before
    /// the loop reads stalled; the grammar, its default and the boot check
    /// against the call-control timeout in `liveness_env`.
    pub loop_stall: Duration,
}

/// `key` parsed as `T`, `default` when unset; `Err` names the variable and
/// its value.
fn knob<T>(key: &str, default: &str) -> Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    let value = env_or(key, default);
    value.parse().map_err(|e| format!("{key}={value:?}: {e}"))
}

/// The socket buffer size a `B2BUA_UDP_*BUF` knob requests, in bytes: 4 MiB
/// when unset, `None` (the kernel default) when set blank.
fn socket_buffer_knob(key: &str) -> Result<Option<usize>, String> {
    stated(Some(env_or(key, "4194304")))
        .map(|s| s.parse().map_err(|e| format!("{key}={s:?}: {e}")))
        .transpose()
}

/// `B2BUA_CDR_MESSAGE_RING` (default `0`, off) and `B2BUA_CDR_CAPTURED_HEADERS`
/// (comma-separated, default none) as `lookup` states them; a blank value is
/// unset. An unparsable cap is the error.
pub(crate) fn cdr_ring_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(usize, Vec<String>), String> {
    let ring = stated(lookup("B2BUA_CDR_MESSAGE_RING"))
        .map(|v| v.parse().map_err(|e| format!("B2BUA_CDR_MESSAGE_RING={v:?}: {e}")))
        .transpose()?
        .unwrap_or(0);
    let headers = stated_list(lookup("B2BUA_CDR_CAPTURED_HEADERS")).unwrap_or_default();
    Ok((ring, headers))
}

/// An on/off knob as `value` states it: truthy or falsy (`0`/`false`/`no`/
/// `off`), `default` when absent or blank. Any other value is the error.
pub(crate) fn switch(key: &str, value: Option<String>, default: bool) -> Result<bool, String> {
    let Some(value) = stated(value) else { return Ok(default) };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{key} must be on or off, got {value:?}")),
    }
}

impl RunnerEnv {
    /// Parse the full generic env grammar. `Err` is the boot refusal of the
    /// first value the worker cannot honour, naming its variable.
    pub fn from_env() -> Result<Self, String> {
        let lookup = |k: &str| env::var(k).ok();
        let invalid = |e: String| format!("invalid B2BUA config: {e}");
        let (cdr_message_ring, cdr_captured_headers) = cdr_ring_from_lookup(lookup)?;
        let limiter = limiter_env::limiter_from_lookup(lookup).map_err(invalid)?;
        let call_control_timeout_ms: i64 = knob("B2BUA_CALL_CONTROL_TIMEOUT_MS", "5000")?;
        let call_control =
            Duration::from_millis(u64::try_from(call_control_timeout_ms).unwrap_or(0));
        let outbound_proxy = match stated(env::var("B2BUA_OUTBOUND_PROXY").ok()) {
            Some(s) => Some(parse_outbound_proxy(&s)?),
            None => None,
        };
        Ok(Self {
            listen: env_or("B2BUA_LISTEN", "0.0.0.0:5060"),
            advertise: env::var("B2BUA_ADVERTISE").ok(),
            outbound_proxy,
            require_outbound_proxy: stated(env::var("B2BUA_REQUIRE_OUTBOUND_PROXY").ok()).is_some(),
            metrics_addr: env_or("B2BUA_METRICS", "0.0.0.0:9091"),
            queue_max: knob("B2BUA_QUEUE", "8192")?,
            udp_sndbuf: socket_buffer_knob("B2BUA_UDP_SNDBUF")?,
            udp_rcvbuf: socket_buffer_knob("B2BUA_UDP_RCVBUF")?,
            cdr_queue: knob("B2BUA_CDR_QUEUE", "1024")?,
            ordinal: env_or("B2BUA_ORDINAL", "w0"),
            identity: stated(env::var("POD_UID").ok()),
            // Dispatch throttle ceilings — deliberately high so they never cap
            // throughput below the offered rate; `cap_drops`/`saturation`
            // metrics flag if either is actually hit.
            concurrency: knob("B2BUA_CONCURRENCY", "8192")?,
            call_cap: knob("B2BUA_CALL_CAP", "1000000")?,
            new_call_permit_share_percent: knob("B2BUA_NEW_CALL_PERMIT_SHARE_PCT", "50")?,
            new_call_queue_headroom_percent: knob("B2BUA_NEW_CALL_QUEUE_HEADROOM_PCT", "5")?,
            max_messages_per_call: knob("B2BUA_MAX_MESSAGES_PER_CALL", "200")?,
            max_messages_per_call_lifetime: knob("B2BUA_MAX_MESSAGES_PER_CALL_LIFETIME", "100000")?,
            keepalive_sec: knob("B2BUA_KEEPALIVE_SEC", "300")?,
            keepalive_timeout_sec: knob("B2BUA_KEEPALIVE_TIMEOUT_SEC", "32")?,
            replica: b2bua::ReplicaConfig {
                reboot_budget_sec: knob("B2BUA_REBOOT_BUDGET_SEC", "600")?,
                backup: capacity_env::backup_from_lookup(lookup).map_err(invalid)?,
                ..Default::default()
            },
            setup_timeout_ms: knob("B2BUA_SETUP_TIMEOUT_MS", "150000")?,
            invite_txn_timeout_sec: knob("B2BUA_INVITE_TXN_TIMEOUT_SEC", "158")?,
            invite_first_response_timeout_sec: knob(
                "B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC",
                "32",
            )?,
            call_control_timeout_ms,
            ack_timeout_sec: knob("B2BUA_ACK_TIMEOUT_SEC", "32")?,
            cps_bucket_size: knob("B2BUA_CPS_BUCKET_SIZE", "1000")?,
            cps_bucket_rate: knob("B2BUA_CPS_BUCKET_RATE", "500")?,
            overload_panic_elu_threshold: knob("B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD", "0.75")?,
            retry_after_base_sec: knob("B2BUA_RETRY_AFTER_BASE_SEC", "5")?,
            worker_allowed_target_suffixes: split_csv(&env_or(
                "WORKER_ALLOWED_TARGET_SUFFIXES",
                ".svc.cluster.local",
            )),
            udp_ingress_brake_pct: knob("B2BUA_UDP_INGRESS_BRAKE_PCT", "70")?,
            retry_after_jitter_sec: knob("B2BUA_RETRY_AFTER_JITTER_SEC", "5")?,
            relay_headers: split_csv(&env_or("B2BUA_RELAY_HEADERS", "")),
            privacy_service: switch(
                "B2BUA_PRIVACY_SERVICE",
                env::var("B2BUA_PRIVACY_SERVICE").ok(),
                true,
            )?,
            cdr_message_ring,
            cdr_captured_headers,
            limiter_url: limiter_env::limiter_url_from_lookup(lookup)
                .map_err(invalid)?
                .unwrap_or_default(),
            limiter_timeout_ms: limiter.timeout_ms,
            limiter_refresh_sec: limiter.refresh_sec,
            limiter_refresh_batch_ms: limiter.refresh_batch_ms,
            limiter_refresh_batch_max: limiter.refresh_batch_max,
            limiter_refresh_timeout_ms: limiter.refresh_timeout_ms,
            limiter_release_timeout_ms: limiter.release_timeout_ms,
            limiter_release_queue_cap: limiter.queue_cap,
            limiter_backend: limiter.backend,
            drain: drain_env::drain_from_lookup(lookup).map_err(invalid)?,
            capacity: capacity_env::capacity_from_lookup(lookup).map_err(invalid)?,
            loop_stall: liveness_env::loop_stall_from_lookup(lookup, call_control)
                .map_err(invalid)?,
        })
    }
}
