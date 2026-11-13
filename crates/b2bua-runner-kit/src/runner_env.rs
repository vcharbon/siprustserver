//! [`RunnerEnv`] — the generic runner knobs and their env grammar, read once.
//! A value the worker cannot honour refuses the boot, naming its variable: a
//! typo'd knob never silently becomes a default.

use std::env;
use std::fmt::Display;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use b2bua::config::{B2buaConfig, CapacityConfig, CdrConfig};
use sip_message::{Exemptions, SipParserLimits};

use crate::{
    addresses_from_lookup, capacity_env, drain_env, env_or, limiter_env, liveness_env, split_csv,
    stated, stated_list, ReplicationSettings, WorkerAddresses,
};

pub(crate) mod caps_env;
mod event_queue_env;
mod watchdog_env;

/// The limits a worker reads the wire under ([`RunnerEnv::parser_limits`]), the
/// exemptions as `SIP_EXEMPTIONS` states them through `lookup`.
pub(crate) fn parser_limits(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<SipParserLimits, String> {
    let exemptions = Exemptions::read(lookup("SIP_EXEMPTIONS").as_deref())
        .map_err(|problem| format!("SIP_EXEMPTIONS {problem}"))?;
    Ok(SipParserLimits { exemptions, ..SipParserLimits::DEFAULT })
}

/// Every generic runner knob, parsed once from env by [`RunnerEnv::from_env`].
/// All fields are pub: tweak any of them between `from_env()` and
/// [`bind`](RunnerEnv::bind) to override a single knob without re-implementing
/// the rest (a hardcoded listen port, a test-only drain grace, …).
pub struct RunnerEnv {
    /// `B2BUA_LISTEN` — SIP/signaling listen addr (default `0.0.0.0:5060`).
    pub listen: String,
    /// `B2BUA_ADVERTISE` and `B2BUA_MODE` with the proxy's addresses: the
    /// worker's own `ip:port` and its deployment mode, read by
    /// [`addresses_from_lookup`], which refuses an inconsistent pair.
    pub addresses: WorkerAddresses,
    /// `B2BUA_METRICS` — Prometheus HTTP listen addr (default `0.0.0.0:9091`).
    pub metrics_addr: String,
    /// `B2BUA_ADMIN` — the operator routes' listen addr, a loopback
    /// `ip:port` only (default `127.0.0.1:9094`).
    pub admin_addr: SocketAddr,
    /// `B2BUA_QUEUE` — inbound UDP queue depth, packets (default 8192).
    pub queue_max: usize,
    /// `B2BUA_CAPTURE` — the pcapng file the worker appends each SIP datagram
    /// it sends and receives to, one section per start
    /// ([`sip_net::CaptureEndpoint`]); unset or blank, nothing is captured.
    /// It refuses the boot where `listen` is the unspecified address.
    pub capture: Option<PathBuf>,
    /// What the process reads the wire under: the default limits and the
    /// exemptions `SIP_EXEMPTIONS` states (comma-separated, default none; a blank
    /// value or a name no exemption has refuses the boot, as on the proxy).
    /// [`bind`](Self::bind) makes the one parser of it.
    pub parser_limits: SipParserLimits,
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
    /// downward API (ADR-0055 §6), required when replication discovers its
    /// peers. Unset in a static pool: a per-boot token drawn from the OS RNG.
    pub identity: Option<String>,
    /// `B2BUA_CLUSTER_KEY` — the secret every worker of the cluster shares to
    /// sign the via-extension on its own requests (ADR-0055 §12), 16 to 64
    /// bytes. Unset: the worker states none and trusts none.
    pub cluster_key: Option<sip_ids::SigningKey>,
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
    /// The replication tunables: `B2BUA_REBOOT_BUDGET_SEC`, the replica TTL /
    /// reboot budget (default 600; with replication on, at least one
    /// keepalive round and above the INVITE bound, `ReplicaConfig::validate`), and the
    /// backup ceilings `B2BUA_REPL_BACKUP_MAX_*` (grammar in `capacity_env`).
    pub replica: b2bua::ReplicaConfig,
    /// `B2BUA_SETUP_TIMEOUT_MS` — a-leg total setup deadline, reroutes
    /// included, in ms (default 150000; must stay strictly below
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC` in the final composed configuration,
    /// checked at spawn; <= 0 disables).
    pub setup_timeout_ms: i64,
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC` — the sip-txn out-of-dialog INVITE bound
    /// for BOTH call halves (b-leg client give-up + a-leg pre-final backstop).
    /// Default `B2buaConfig::DEFAULT_INVITE_TXN_TIMEOUT_SEC` (158); supported
    /// range 33..=600 (telephony deployments raise it — Timer C > 3 min, 180 s
    /// PSTN supervision). The spawn check refuses boot when the final
    /// `B2BUA_SETUP_TIMEOUT_MS` does not sit strictly below it.
    pub invite_txn_timeout_sec: i64,
    /// `B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC` — how long the worker waits
    /// for a response of ANY kind to a b-leg initial INVITE before it gives up
    /// on the hop and the `call_failure` consult carries `timeout_kind:
    /// "response"` (ADR-0044). Default 5; supported range
    /// 2..=32. In-dialog INVITE and non-INVITE never read it.
    pub invite_first_response_timeout_sec: i64,
    /// `B2BUA_CALL_CONTROL_TIMEOUT_MS` — the setup budget from an initial
    /// INVITE's receipt to its final, an off-turn consult's deadline per round
    /// trip (default 5000; <= 0 is refused at startup — ADR-0022 X1).
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
    /// `SIP_TRACE_HEADER` — on: an `X-Trace-Sample` header may sample a call
    /// (default off, a lab setting); any other value than on/off refuses the
    /// boot.
    pub trace_header: bool,
    /// The call record's message ring: `B2BUA_CDR_MESSAGE_RING` (the
    /// per-leg cap, default 0 = off), `B2BUA_CDR_CAPTURED_HEADERS` (the
    /// names every entry captures, default none) and
    /// `B2BUA_CDR_FINAL_RESPONSE_HEADERS` (the names an INVITE final's
    /// entry captures instead, default the captured ones); blank = unset.
    pub cdr: CdrConfig,
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
    /// The per-call caps (ADR-0057), each at its upstream default unless
    /// stated; the grammar and the boot checks are in `caps_env`.
    pub caps: b2bua::config::CallCaps,
    /// How long one turn of the transaction owner or router may run before
    /// the loop reads stalled; the grammar, its default and the boot check
    /// against the call-control timeout in `liveness_env`.
    pub loop_stall: Duration,
    /// How long one such turn may run before the worker exits non-zero; the
    /// grammar, its default and the boot check in `liveness_env`.
    pub loop_exit: Duration,
    /// The CPU one poll may spend on one runtime thread before the worker
    /// discharges the call it serves and exits (ADR-0057 §6); the grammar in
    /// `watchdog_env`.
    pub stuck_poll_cpu: Duration,
    /// The event-queue gate's bounds (ADR-0050 §9), the grammar, defaults
    /// and boot checks in `runner_env::event_queue_env`.
    pub event_queue_gate: b2bua::config::EventQueueGateConfig,
    /// `B2BUA_REPL` and the replication grammar ([`ReplicationSettings`]);
    /// `None` when replication is off.
    pub replication: Option<ReplicationSettings>,
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

/// `B2BUA_CDR_MESSAGE_RING` (default `0`, off), `B2BUA_CDR_CAPTURED_HEADERS`
/// (comma-separated, default none) and `B2BUA_CDR_FINAL_RESPONSE_HEADERS`
/// (comma-separated, default the captured ones) as `lookup` states them; a
/// blank value, or a list naming no header, is unset. An unparsable cap is
/// the error.
pub(crate) fn cdr_ring_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<CdrConfig, String> {
    let message_ring = stated(lookup("B2BUA_CDR_MESSAGE_RING"))
        .map(|v| v.parse().map_err(|e| format!("B2BUA_CDR_MESSAGE_RING={v:?}: {e}")))
        .transpose()?
        .unwrap_or(0);
    Ok(CdrConfig {
        message_ring,
        captured_headers: stated_list(lookup("B2BUA_CDR_CAPTURED_HEADERS")).unwrap_or_default(),
        final_response_headers: stated_list(lookup("B2BUA_CDR_FINAL_RESPONSE_HEADERS")),
    })
}

/// An on/off knob as `value` states it: truthy or falsy (`0`/`false`/`no`/
/// `off`), `default` when absent or blank. Any other value is the error.
pub fn switch(key: &str, value: Option<String>, default: bool) -> Result<bool, String> {
    let Some(value) = stated(value) else { return Ok(default) };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{key} must be on or off, got {value:?}")),
    }
}

/// `B2BUA_CLUSTER_KEY` as the key it holds: unset is none, a value out of
/// the key's bounds refuses the boot.
fn cluster_key(value: Option<String>) -> Result<Option<sip_ids::SigningKey>, String> {
    let Some(value) = value else { return Ok(None) };
    sip_ids::SigningKey::new(value.as_bytes()).map(Some).ok_or_else(|| {
        format!(
            "B2BUA_CLUSTER_KEY must hold {} to {} bytes, got {}",
            sip_ids::MIN_KEY_BYTES,
            sip_ids::MAX_KEY_BYTES,
            value.len()
        )
    })
}

/// `B2BUA_ADMIN`: a loopback `ip:port`, `127.0.0.1:9094` when unset. `Err`
/// names the variable for a value that is no address or not a loopback one.
pub(crate) fn admin_addr(lookup: impl Fn(&str) -> Option<String>) -> Result<SocketAddr, String> {
    let raw = stated(lookup("B2BUA_ADMIN")).unwrap_or_else(|| "127.0.0.1:9094".into());
    match raw.trim().parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => Ok(addr),
        Ok(addr) => Err(format!(
            "B2BUA_ADMIN={addr} is not a loopback address: the operator routes are reached \
             from inside the worker's network namespace only"
        )),
        Err(e) => Err(format!("B2BUA_ADMIN={raw:?} is not an ip:port: {e}")),
    }
}

impl RunnerEnv {
    /// Parse the full generic env grammar. `Err` is the boot refusal of the
    /// first value the worker cannot honour, naming its variable.
    pub fn from_env() -> Result<Self, String> {
        let lookup = |k: &str| env::var(k).ok();
        let invalid = |e: String| format!("invalid B2BUA config: {e}");
        let cdr = cdr_ring_from_lookup(lookup)?;
        let limiter = limiter_env::limiter_from_lookup(lookup).map_err(invalid)?;
        let call_control_timeout_ms: i64 = knob(
            "B2BUA_CALL_CONTROL_TIMEOUT_MS",
            &B2buaConfig::DEFAULT_CALL_CONTROL_TIMEOUT_MS.to_string(),
        )?;
        let call_control = B2buaConfig::call_control_timeout_of(call_control_timeout_ms);
        let loop_stall =
            liveness_env::loop_stall_from_lookup(lookup, call_control).map_err(invalid)?;
        let addresses = addresses_from_lookup(lookup)?;
        Ok(Self {
            listen: env_or("B2BUA_LISTEN", "0.0.0.0:5060"),
            addresses,
            metrics_addr: env_or("B2BUA_METRICS", "0.0.0.0:9091"),
            admin_addr: admin_addr(lookup)?,
            queue_max: knob("B2BUA_QUEUE", "8192")?,
            capture: stated(lookup("B2BUA_CAPTURE")).map(PathBuf::from),
            parser_limits: parser_limits(lookup)?,
            udp_sndbuf: socket_buffer_knob("B2BUA_UDP_SNDBUF")?,
            udp_rcvbuf: socket_buffer_knob("B2BUA_UDP_RCVBUF")?,
            cdr_queue: knob("B2BUA_CDR_QUEUE", "1024")?,
            ordinal: env_or("B2BUA_ORDINAL", "w0"),
            identity: stated(env::var("POD_UID").ok()),
            cluster_key: cluster_key(stated(env::var("B2BUA_CLUSTER_KEY").ok()))?,
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
            replica: b2bua::ReplicaConfig {
                reboot_budget_sec: knob("B2BUA_REBOOT_BUDGET_SEC", "600")?,
                backup: capacity_env::backup_from_lookup(lookup).map_err(invalid)?,
                ..Default::default()
            },
            setup_timeout_ms: knob("B2BUA_SETUP_TIMEOUT_MS", "150000")?,
            invite_txn_timeout_sec: knob(
                "B2BUA_INVITE_TXN_TIMEOUT_SEC",
                &B2buaConfig::DEFAULT_INVITE_TXN_TIMEOUT_SEC.to_string(),
            )?,
            invite_first_response_timeout_sec: knob(
                "B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC",
                "5",
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
            trace_header: switch(
                observe::TRACE_HEADER_ENV,
                env::var(observe::TRACE_HEADER_ENV).ok(),
                false,
            )?,
            cdr,
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
            caps: caps_env::caps_from_lookup(lookup).map_err(invalid)?,
            loop_stall,
            loop_exit: liveness_env::loop_exit_from_lookup(lookup, loop_stall).map_err(invalid)?,
            stuck_poll_cpu: watchdog_env::stuck_poll_cpu_from_lookup(lookup).map_err(invalid)?,
            event_queue_gate: event_queue_env::event_queue_gate_from_lookup(lookup)
                .map_err(invalid)?,
            replication: ReplicationSettings::from_lookup(lookup).map_err(invalid)?,
        })
    }
}
