//! Standalone, containerizable SIP front-proxy process.
//!
//! Wires the `sip-proxy` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork`) into the production data path:
//!   - `LoadBalancerStrategy` — HRW worker selection + HMAC-signed Record-Route
//!     stickiness cookie (rotation-overlap aware).
//!   - `StaticWorkerRegistry` — worker pool from `id@host:port,...` (the k8s
//!     watcher registry is a deferred slice).
//!   - `HealthProbe` — periodic OPTIONS to workers, feeding the
//!     `WorkerLoadObserver` band classifier (above-critical workers are excluded
//!     from new-dialog selection). Health writes flow through the registry's
//!     `control()` seam, so an unanswered worker is demoted (Dead/NotReady/
//!     Draining) and routing reacts — for the static pool too.
//!   - `EluCpsGate` self-gate (migration/14): EWMA-smoothed proxy-self ELU + a
//!     per-class CPS token bucket shed external new-dialog non-emergency INVITEs
//!     under self-overload (a stateless 503 + `Retry-After`/`Reason`). A 100 ms
//!     `tokio::time::interval` task drives its load sampler. `PROXY_SELF_GATE=0`
//!     reverts to the always-admit gate.
//!   - Prometheus `/metrics` + `/healthz` + `/readyz` via the shared `probe-http`.
//!
//! Config via env (all optional):
//!   PROXY_LISTEN     SIP listen addr                       (default 0.0.0.0:5060)
//!   PROXY_ADVERTISE  host:port stamped on Via/Record-Route (default: listen, or
//!                    127.0.0.1 if listen IP is unspecified)
//!   PROXY_WORKERS    static worker pool "id@host:port,..." (default empty → k8s
//!                    EndpointSlice discovery; ADR-0012 D4)
//!   PROXY_WORKER_SERVICE  headless worker Service to watch  (default b2bua-worker)
//!   PROXY_WORKER_PORT     SIP port appended to each Pod IP  (default 5060)
//!   PROXY_NAMESPACE  namespace for k8s discovery            (default $POD_NAMESPACE / sip-test)
//!   PROXY_METRICS    Prometheus HTTP listen addr           (default 0.0.0.0:9090)
//!   PROXY_QUEUE      inbound UDP queue depth (packets)      (default 8192)
//!   PROXY_RECV_SHARDS  N reuse-port recv sockets/cores      (default 1; >1 shards
//!                    the serial recv loop — the ~550 OPTIONS/s burst ceiling —
//!                    across N sockets; kernel flow-hashing keeps each UAC
//!                    src:port on one socket so per-flow ordering holds)
//!   PROXY_HMAC_KID   stickiness cookie key id              (default k0)
//!   PROXY_HMAC_KEY   stickiness cookie secret (>=16 bytes) (default dev key)
//!   HEALTH_INTERVAL_MS / HEALTH_TIMEOUT_MS / HEALTH_THRESHOLD  (probe tuning)
//!   PROXY_SELF_GATE  1 → real ELU/CPS self-gate, 0 → always-admit (default 1)
//!   PROXY_SELF_GATE_ELU_CRITICAL  ELU shed threshold       (default 0.80)
//!   PROXY_SELF_GATE_CPS_SIZE      CPS bucket capacity      (default 50)
//!   PROXY_SELF_GATE_CPS_RATE      CPS bucket refill /s     (default 100)

// Use jemalloc instead of glibc malloc (mirrors b2bua-runner). glibc retains
// freed arena chunks and ratchets RSS under sustained churn; jemalloc's decay
// purging returns pages to the OS and bounds steady-state RSS. The proxy is
// single-task and far less alloc-heavy than the b2bua, so this is mostly for a
// uniform memory story across tiers — but it also lets the same jemalloc_*
// /metrics probe rule the proxy in or out during a soak. Tuned via
// _RJEM_MALLOC_CONF on the container (see deploy/k8s/manifests/30-proxy.yaml).
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::composed::ComposedWorkerRegistry;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};
use topology::{K8sMembership, Membership};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::self_gate::{AlwaysAdmitGate, EluCpsGate, ProxySelfGate, ProxySelfGateConfig};
use sip_proxy::strategies::{LoadBalancerConfig, LoadBalancerStrategy};
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, RoutingStrategy};
use sip_txn::IdGen;

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| panic!("cannot resolve {addr:?}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("no address resolved from {addr:?}"))
}

fn parse_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn parse_f64(key: &str, default: f64) -> f64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// `1`/`true`/`yes`/`on` → on; `0`/`false`/`no`/`off` → off; anything else uses
/// `default`. Used for the `PROXY_SELF_GATE` toggle.
fn parse_bool(key: &str, default: bool) -> bool {
    match env::var(key).ok().as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

/// Subset of [`HealthProbeConfig`] timings the cross-component preflight needs.
/// Kept as a bare pair (not the full config) so [`validate_config`] stays
/// decoupled from probe-layer construction — mirrors the TS `ProbeTimingConfig`.
#[derive(Debug, Clone, Copy)]
struct ProbeTimingConfig {
    interval_ms: u64,
    timeout_ms: u64,
}

/// Cross-component config validation for the SIP front proxy (pure port of
/// `src/sip-front-proxy/config-validation.ts`).
///
/// Some invariants live BETWEEN components (`HealthProbe` ↔ `WorkerLoadObserver`)
/// and cannot be enforced by either component's own defaults in isolation: by the
/// time the observer is built, the probe timings have already been handed to the
/// probe layer and are no longer visible to it. This assembles the cross-cutting
/// facts and rejects misconfigurations that have caused real outages. It is
/// deliberately **pure** (no I/O, no logging, no `tokio`) so it is trivial to
/// unit-test and so [`assert_valid_config`] can abort boot *before* a single
/// socket is bound — a misconfigured pod MUST NOT come up unhealthy.
///
/// Background: on 2026-05-25 the deployed chart used `interval_ms=2000
/// timeout_ms=1500` (cycle=3500 ms) but the observer shipped with
/// `payload_stale_ms=3000`. The 1 s sweep caught `age > 3000` between probe-recv
/// events on ~half of cycles, halving the per-worker admit cap each time; within
/// ~20 s the cap collapsed to `cap_floor_cps=1` and never recovered — the cluster
/// silently throttled every worker to 1 cps per LB. The `payload_stale_ms ≥ 2 ×
/// probe_cycle` check below would have refused to start that pod.
///
/// Returns `Ok(())` when every invariant holds, else `Err(violations)` with one
/// human-readable line per problem (operators see ALL problems on the first boot
/// attempt, not one at a time).
fn validate_config(
    probe: ProbeTimingConfig,
    observer: &LoadObserverConfig,
) -> Result<(), Vec<String>> {
    let mut violations: Vec<String> = Vec::new();

    // Each timing individually positive — guards a typo (e.g. 0 ms timeout) that
    // would otherwise produce confusing downstream behaviour. `u64` interval/
    // timeout cannot be NaN/negative/infinite (unlike the TS `number`), so only
    // the `== 0` arm of the TS positivity guard is reachable here.
    if probe.interval_ms == 0 {
        violations.push(format!(
            "health_probe.interval_ms must be a positive number (got {}).",
            probe.interval_ms,
        ));
    }
    if probe.timeout_ms == 0 {
        violations.push(format!(
            "health_probe.timeout_ms must be a positive number (got {}).",
            probe.timeout_ms,
        ));
    }
    if observer.payload_stale_ms <= 0 {
        violations.push(format!(
            "worker_load_observer.payload_stale_ms must be positive (got {}).",
            observer.payload_stale_ms,
        ));
    }

    // PRIMARY INVARIANT — the bug from the 2026-05-25 RCA.
    //
    // `HealthProbe` runs sleep(interval) → fanOutOptions → sleep(timeout) → reap,
    // so successive OPTIONS replies arrive `interval + timeout` apart. If
    // `payload_stale_ms < probe_cycle`, `sweep_stale` fires during the gap,
    // halves the cap, and the next probe-recv overwrites `last_action` so the
    // event is invisible. The cap collapses to `cap_floor_cps` in ~5 cycles.
    //
    // `2×` provides one full cycle of safety margin for probe jitter (event-loop
    // pressure, packet loss, retries). `1.5×` still races with the sweep phase;
    // `2×` is the smallest value that survives a single dropped probe without
    // triggering the floor cascade.
    let probe_cycle_ms = probe.interval_ms.saturating_add(probe.timeout_ms);
    let min_stale = 2u64.saturating_mul(probe_cycle_ms);
    // payload_stale_ms is `> 0` past the positivity guard above; compare in i64.
    if observer.payload_stale_ms > 0 && (observer.payload_stale_ms as u64) < min_stale {
        violations.push(format!(
            "worker_load_observer.payload_stale_ms ({} ms) must be at least 2× the \
             HealthProbe cycle (interval_ms + timeout_ms = {} + {} = {} ms; minimum \
             allowed payload_stale_ms = {} ms). Otherwise sweep_stale halves the \
             per-worker admit cap on most probe cycles and the cluster silently \
             throttles every worker to cap_floor_cps. See crates/sip-proxy-runner/\
             src/main.rs (validate_config) for the calibration rationale.",
            observer.payload_stale_ms,
            probe.interval_ms,
            probe.timeout_ms,
            probe_cycle_ms,
            min_stale,
        ));
    }

    // Cap-band sanity. Cheap typo guards.
    if observer.cap_floor_cps <= 0.0 {
        violations.push(format!(
            "cap_floor_cps must be > 0 (got {}).",
            observer.cap_floor_cps,
        ));
    }
    if observer.cap_floor_cps > observer.cap_initial_cps {
        violations.push(format!(
            "cap_floor_cps ({}) must be <= cap_initial_cps ({}).",
            observer.cap_floor_cps, observer.cap_initial_cps,
        ));
    }
    if observer.cap_initial_cps > observer.cap_ceiling_cps {
        violations.push(format!(
            "cap_initial_cps ({}) must be <= cap_ceiling_cps ({}).",
            observer.cap_initial_cps, observer.cap_ceiling_cps,
        ));
    }

    // ELU band ordering + hysteresis bounds. Owned by the band classifier
    // (`compute_band`), so the rule lives next to it in `sip-proxy`'s
    // `LoadObserverConfig::validate_bands` and is reused verbatim here.
    observer.validate_bands(&mut violations);

    // Cooldown must outlast at least one probe cycle. A shorter cooldown lets the
    // AIMD ladder yo-yo on a single noisy probe; longer than one cycle is what
    // the cooldown is *for*.
    let cooldown_ms = observer.aimd_cooldown_ticks * observer.options_interval_ms as f64;
    if cooldown_ms < probe_cycle_ms as f64 {
        violations.push(format!(
            "aimd cooldown ({} × {} = {} ms) must be >= one HealthProbe cycle ({} ms) \
             so a single decrease isn't immediately followed by an increase tick.",
            observer.aimd_cooldown_ticks,
            observer.options_interval_ms,
            cooldown_ms,
            probe_cycle_ms,
        ));
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Boot-time assertion: panics (→ non-zero exit → `CrashLoopBackOff`) when the
/// config is invalid, so a misconfigured pod exits instead of booting unhealthy.
/// The message is structured for log scraping — each violation on its own line
/// after a fixed prefix (port of the TS `assertValidProxyConfig`). Called from
/// `main` before any socket bind.
fn assert_valid_config(probe: ProbeTimingConfig, observer: &LoadObserverConfig) {
    if let Err(violations) = validate_config(probe, observer) {
        let header = "sip-front-proxy: refusing to start — invalid configuration:";
        let bullets =
            violations.iter().map(|v| format!("  - {v}")).collect::<Vec<_>>().join("\n");
        panic!("{header}\n{bullets}");
    }
}

/// Read an `f64` env override: `Some(v)` iff the var is set AND parses to a
/// finite number, else `None` (unset OR malformed — both leave the default).
/// A malformed value is logged (so a typo is visible) but does NOT abort boot;
/// the coherence preflight is what rejects an incoherent *combination*.
fn env_f64_opt(key: &str) -> Option<f64> {
    match env::var(key) {
        Ok(raw) => match raw.trim().parse::<f64>() {
            Ok(v) if v.is_finite() => Some(v),
            _ => {
                eprintln!("sip-proxy-runner: ignoring malformed {key}={raw:?} (not a finite number)");
                None
            }
        },
        Err(_) => None,
    }
}

/// Apply the operator's band/AIMD/cap env overrides onto a base
/// [`LoadObserverConfig`] (migration/32). Each `LB_*` var, when present and
/// parseable, replaces one field; anything else leaves the shipped default. The
/// result is NOT validated here — `assert_valid_config` does that (so an
/// incoherent override set fails the boot preflight loudly). Every applied
/// override is logged so the running config is visible in the pod log.
///
/// Exposed env vars (all `f64` cps/ratio):
///   - `LB_ELU_SOFT` / `LB_ELU_HARD` / `LB_ELU_CRITICAL` — band thresholds.
///   - `LB_BAND_HYSTERESIS` — band-boundary hysteresis.
///   - `LB_AIMD_DECREASE_FACTOR` — multiplicative decrease (e.g. 0.5).
///   - `LB_AIMD_INCREASE_STEP_CPS` — additive increase per tick.
///   - `LB_CAP_CEILING_CPS` / `LB_CAP_FLOOR_CPS` — per-worker cap bounds.
fn load_observer_cfg_from_env(mut cfg: LoadObserverConfig) -> LoadObserverConfig {
    let mut applied: Vec<String> = Vec::new();
    let mut set = |name: &str, field: &mut f64| {
        if let Some(v) = env_f64_opt(name) {
            *field = v;
            applied.push(format!("{name}={v}"));
        }
    };
    set("LB_ELU_SOFT", &mut cfg.elu_soft);
    set("LB_ELU_HARD", &mut cfg.elu_hard);
    set("LB_ELU_CRITICAL", &mut cfg.elu_critical);
    set("LB_BAND_HYSTERESIS", &mut cfg.band_hysteresis);
    set("LB_AIMD_DECREASE_FACTOR", &mut cfg.aimd_decrease_factor);
    set("LB_AIMD_INCREASE_STEP_CPS", &mut cfg.aimd_increase_step_cps);
    set("LB_CAP_CEILING_CPS", &mut cfg.cap_ceiling_cps);
    set("LB_CAP_FLOOR_CPS", &mut cfg.cap_floor_cps);
    if applied.is_empty() {
        eprintln!("sip-proxy-runner: load-observer bands = shipped defaults (no LB_* overrides)");
    } else {
        eprintln!("sip-proxy-runner: load-observer band/AIMD overrides applied: {}", applied.join(" "));
    }
    cfg
}

/// Render the proxy-self gate's gauges/counters as Prometheus exposition,
/// appended to the `/metrics` body (mirrors how `jemalloc_stats::prometheus_text`
/// is appended). Only the real [`EluCpsGate`] has state to surface; with the
/// always-admit gate (`None`) this returns empty.
fn self_gate_prometheus_text(gate: &Option<EluCpsGate>) -> String {
    let Some(g) = gate else {
        return String::new();
    };
    let m = g.metrics();
    let mut s = String::new();
    let gauge = |s: &mut String, name: &str, help: &str, val: f64| {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {val}\n"));
    };
    let counter = |s: &mut String, name: &str, help: &str, val: u64| {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"));
    };
    gauge(&mut s, "sip_proxy_self_elu_ewma", "Proxy-self ELU EWMA (0..1). Crosses elu_critical -> 503.", m.elu_ewma);
    gauge(&mut s, "sip_proxy_self_gc_fraction", "Proxy-self GC fraction (0..1). Informational only.", m.gc_fraction);
    gauge(&mut s, "sip_proxy_self_cps_bucket_level", "Proxy-self CPS bucket level (tokens remaining).", m.cps_bucket_level);
    gauge(&mut s, "sip_proxy_self_cps_bucket_max", "Proxy-self CPS bucket capacity (constant per config).", m.cps_bucket_max);
    counter(&mut s, "sip_proxy_self_external_invites_admitted_total", "External new-dialog non-emergency INVITEs admitted by the proxy-self gate.", m.external_admitted_total);
    // Per-reason rejection split (reason=proxy_overload_elu | proxy_overload_cps).
    s.push_str("# HELP sip_proxy_self_external_invites_rejected_total External new-dialog non-emergency INVITEs rejected by the proxy-self gate.\n");
    s.push_str("# TYPE sip_proxy_self_external_invites_rejected_total counter\n");
    s.push_str(&format!("sip_proxy_self_external_invites_rejected_total{{reason=\"proxy_overload_elu\"}} {}\n", m.rejected_elu_total));
    s.push_str(&format!("sip_proxy_self_external_invites_rejected_total{{reason=\"proxy_overload_cps\"}} {}\n", m.rejected_cps_total));
    counter(&mut s, "sip_proxy_self_emergency_bypassed_total", "Emergency INVITEs that bypassed the proxy-self gate.", m.emergency_bypassed_total);
    counter(&mut s, "sip_proxy_self_internal_bypassed_total", "Worker-originated INVITEs that bypassed the proxy-self gate.", m.internal_bypassed_total);
    s
}

/// Build the worker pool registry + its health-write control seam.
///
/// `PROXY_WORKERS` (a static `id@host:port,..` list) takes precedence for
/// dev/local. When empty, the pool is discovered from k8s EndpointSlices via the
/// shared `topology::K8sMembership` informer (ADR-0012 D4) — the same watch the
/// b2bua replication engine consumes; the proxy still reaches each worker by its
/// Pod IP at `PROXY_WORKER_PORT`. If no kube client is available either, falls
/// back to a single loopback worker so the binary still runs locally.
async fn build_registry(
    workers: &str,
    clock: Clock,
) -> (Arc<dyn WorkerRegistry>, Arc<dyn WorkerRegistryControl>) {
    if !workers.trim().is_empty() {
        let reg = StaticWorkerRegistry::from_string_with_clock(workers, "PROXY_WORKERS", clock)
            .unwrap_or_else(|e| panic!("bad PROXY_WORKERS {workers:?}: {e}"));
        let control = reg.control();
        eprintln!("sip-proxy-runner worker pool: static PROXY_WORKERS={workers}");
        return (Arc::new(reg), control);
    }

    let service = env_or("PROXY_WORKER_SERVICE", "b2bua-worker");
    let namespace = env::var("PROXY_NAMESPACE")
        .or_else(|_| env::var("POD_NAMESPACE"))
        .unwrap_or_else(|_| "sip-test".to_string());
    let sip_port: u16 = env_or("PROXY_WORKER_PORT", "5060").parse().expect("PROXY_WORKER_PORT");

    // rustls 0.23 ships no default CryptoProvider; install ring once before the
    // kube client opens its first TLS connection (idempotent — a second call Errs,
    // which we ignore).
    let _ = rustls::crypto::ring::default_provider().install_default();
    match kube::Client::try_default().await {
        Ok(client) => {
            eprintln!(
                "sip-proxy-runner worker pool: k8s EndpointSlice informer (svc={service}, ns={namespace}, port={sip_port})"
            );
            let membership: Arc<dyn Membership> =
                Arc::new(K8sMembership::spawn(client, namespace, service));
            let reg = ComposedWorkerRegistry::spawn(membership, sip_port, clock);
            let control = reg.control();
            (Arc::new(reg), control)
        }
        Err(e) => {
            let fallback = "w0@127.0.0.1:5060";
            eprintln!(
                "sip-proxy-runner no kube client ({e}) and no PROXY_WORKERS — falling back to {fallback}"
            );
            let reg =
                StaticWorkerRegistry::from_string(fallback, "fallback").expect("fallback pool");
            let control = reg.control();
            (Arc::new(reg), control)
        }
    }
}

#[tokio::main]
async fn main() {
    // Loud confirmation the jemalloc decay config (_RJEM_MALLOC_CONF) parsed —
    // a typo is silently ignored. Mirrored by the jemalloc_opt_*_decay_ms gauges.
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();

    let listen = env_or("PROXY_LISTEN", "0.0.0.0:5060");
    // PROXY_WORKERS (static `id@host:port,..`) takes precedence (dev/local). When
    // empty, the worker pool is discovered from k8s EndpointSlices (ADR-0012 D4).
    let workers = env_or("PROXY_WORKERS", "");
    let metrics_addr = env_or("PROXY_METRICS", "0.0.0.0:9090");
    let queue_max: usize = env_or("PROXY_QUEUE", "8192").parse().expect("PROXY_QUEUE");
    let hmac_kid = env_or("PROXY_HMAC_KID", "k0");
    let hmac_key = env_or("PROXY_HMAC_KEY", "dev-stickiness-key-not-for-prod-0123456789");

    let listen_sa = resolve(&listen);
    let metrics_sa = resolve(&metrics_addr);

    // Advertised host:port (stamped on Via/Record-Route, must be reachable by the
    // UAC for in-dialog stickiness). Default to the listen addr; if the listen IP
    // is unspecified (0.0.0.0), fall back to loopback so the literal is routable.
    let advertised = match env::var("PROXY_ADVERTISE") {
        Ok(s) => ProxyAddr::parse(&s).unwrap_or_else(|| panic!("bad PROXY_ADVERTISE {s:?}")),
        Err(_) => {
            let ip = if listen_sa.ip().is_unspecified() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                listen_sa.ip()
            };
            ProxyAddr::new(ip.to_string(), listen_sa.port())
        }
    };

    let recv_shards: usize = env_or("PROXY_RECV_SHARDS", "1").parse().expect("PROXY_RECV_SHARDS");
    assert!(recv_shards >= 1, "PROXY_RECV_SHARDS must be >= 1");

    // Cross-component config preflight (port of config-validation.ts). Resolve
    // the HealthProbe timings + the WorkerLoadObserver config here so the
    // BETWEEN-component invariants (payload_stale_ms ≥ 2× probe cycle, cap floor/
    // initial/ceiling ordering, ELU band ordering + hysteresis bounds, cooldown ≥
    // one probe cycle) can be checked *before* the first socket bind. A violation
    // panics → non-zero exit → CrashLoopBackOff, rather than letting a pod boot
    // unhealthy (the 2026-05-25 RCA: a too-short payload_stale_ms silently floored
    // every worker's admit cap to cap_floor_cps). These same values are reused for
    // the observer/probe below — read once.
    let probe_cfg = HealthProbeConfig {
        interval_ms: parse_u64("HEALTH_INTERVAL_MS", 1_000),
        timeout_ms: parse_u64("HEALTH_TIMEOUT_MS", 1_500),
        threshold: parse_u64("HEALTH_THRESHOLD", 2) as u32,
    };
    // Operator env surface for the band/AIMD/cap knobs (migration/32). Overload
    // calibration is iterative on the cluster, so these must be tunable WITHOUT a
    // rebuild. Each override is applied only when its env var is set AND parses;
    // a malformed value is ignored (the shipped default stays). An INCOHERENT
    // result (reversed bands, hysteresis ≥ a band gap, cap ordering) is caught
    // loudly by `assert_valid_config` below — it refuses to boot rather than
    // silently mis-shedding (commit e0b17d7 coherence checks).
    let observer_cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
    assert_valid_config(
        ProbeTimingConfig { interval_ms: probe_cfg.interval_ms, timeout_ms: probe_cfg.timeout_ms },
        &observer_cfg,
    );

    let net = RealSignalingNetwork::new();

    // Main signaling endpoint(s) — one per recv shard. With N > 1 every bind
    // (including the first) sets SO_REUSEPORT; the kernel flow-hashes on the
    // 4-tuple so all datagrams from one UAC src:port land on ONE socket and
    // per-flow ordering (INVITE→CANCEL, retransmits) is preserved.
    let mut endpoints = Vec::with_capacity(recv_shards);
    for _ in 0..recv_shards {
        let ep = net
            .bind_udp(BindUdpOpts::new(listen_sa, queue_max).with_reuse_port(recv_shards > 1))
            .await
            .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"));
        endpoints.push(ep);
    }

    // Separate endpoint for the OPTIONS health probe (its own source socket).
    let probe_ep = net
        .bind_udp(BindUdpOpts::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            1024,
        ))
        .await
        .unwrap_or_else(|e| panic!("bind probe socket failed: {e:?}"));

    let hmac = Arc::new(
        StaticHmacKeyProvider::new(HmacKey::new(hmac_kid, hmac_key.into_bytes()), None)
            .unwrap_or_else(|e| panic!("bad PROXY_HMAC_KEY: {e}")),
    );
    let observer = Arc::new(WorkerLoadObserver::new(observer_cfg));
    let metrics = Arc::new(ProxyMetrics::new());
    let clock = Clock::system();
    let id_gen = Arc::new(IdGen::from_entropy());

    // Proxy-self ELU/CPS admission gate (migration/14). On by default; the live
    // ELU sampler is driven by the 100 ms task spawned below. `PROXY_SELF_GATE=0`
    // reverts to the always-admit gate (then `self_gate` is `None`, no sampler
    // task, no self-gate /metrics). Build the concrete `EluCpsGate` once so the
    // sampler task + the /metrics exposition can read it; hand each core an
    // `Arc<dyn ProxySelfGate>` view of the same gate.
    let self_gate: Option<EluCpsGate> = if parse_bool("PROXY_SELF_GATE", true) {
        Some(EluCpsGate::live_with(ProxySelfGateConfig {
            elu_critical: parse_f64("PROXY_SELF_GATE_ELU_CRITICAL", 0.8),
            cps_bucket_size: parse_u64("PROXY_SELF_GATE_CPS_SIZE", 50) as u32,
            cps_bucket_rate: parse_u64("PROXY_SELF_GATE_CPS_RATE", 100) as u32,
            ..ProxySelfGateConfig::default()
        }))
    } else {
        None
    };
    let gate_dyn: Arc<dyn ProxySelfGate> = match &self_gate {
        Some(g) => Arc::new(g.clone()),
        None => Arc::new(AlwaysAdmitGate),
    };

    // Worker pool + its health-write seam. Static PROXY_WORKERS (dev/local) or the
    // k8s EndpointSlice informer (ADR-0012 D4). Either way the OPTIONS probe writes
    // observed health through `health_control` so an unanswered worker is demoted
    // (Dead/NotReady/Draining) and in-dialog requests fail over to the backup.
    let (registry, health_control) = build_registry(&workers, clock.clone()).await;

    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac,
        observer.clone(),
        metrics.clone(),
        clock.clone(),
        LoadBalancerConfig::default(),
    ));

    // Health probe: periodic OPTIONS to the pool, feeding the band observer.
    // Clone the observer first — the probe takes ownership of one `Arc` view, the
    // stale-payload sweep task below shares the SAME `Arc<WorkerLoadObserver>`.
    let sweep_observer = observer.clone();
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        health_control,
        observer,
        clock.clone(),
        id_gen.clone(),
        probe_cfg,
    )
    // Per-peer probe-miss attribution (sip_proxy_peer_failures_total{
    // scope="internal",kind="response_timeout"}): worker OPTIONS timeouts.
    .with_metrics(metrics.clone());

    // N recv-shard cores over the shared routing state: strategy/registry/
    // metrics are `Arc`s, and the CANCEL/rtx LRU MUST be one instance across
    // shards — a response or CANCEL can hash to a different socket than the
    // INVITE that populated the entry.
    let cancel_lru = Arc::new(sip_proxy::cancel_lru::CancelBranchLru::with_clock(clock.clone()));
    let mut cores = Vec::with_capacity(recv_shards);
    for (shard, endpoint) in endpoints.into_iter().enumerate() {
        cores.push(
            ProxyCoreBuilder::new(advertised.clone(), strategy.clone(), registry.clone())
                .clock(clock.clone())
                .id_gen(id_gen.clone())
                .metrics(metrics.clone())
                .cancel_lru(cancel_lru.clone())
                .self_gate(gate_dyn.clone())
                .shard(shard)
                .build(endpoint),
        );
    }

    eprintln!(
        "sip-proxy-runner pid={} listening UDP {listen_sa} advertise={}:{} workers=[{}] (queue={queue_max}, recv_shards={recv_shards})",
        std::process::id(),
        advertised.host,
        advertised.port,
        registry
            .snapshot()
            .iter()
            .map(|w| format!("{}@{}:{}", w.id, w.address.host, w.address.port))
            .collect::<Vec<_>>()
            .join(","),
    );

    // Readiness gate (ADR-0012 D4): the proxy is fit to take traffic only with
    // ≥1 routable (`Alive`) worker. With the k8s EndpointSlice informer the pool
    // starts empty and fills asynchronously (and stays empty if the watch is
    // RBAC-forbidden / the Service name is wrong), so gating `/readyz` on this
    // keeps the k8s Service from forwarding INVITEs into an empty pool — and turns
    // a misconfigured watch into a loud rollout timeout instead of a silent
    // black-hole. Mirrors the worker's `/ready`.
    // SIGTERM latches this; `/readyz` then reports Draining so k8s unpublishes
    // the proxy from its Service and stops routing new datagrams here.
    let draining = Arc::new(AtomicBool::new(false));
    let ready: probe_http::ReadyFn = {
        let reg = registry.clone();
        let draining = draining.clone();
        Arc::new(move || {
            if draining.load(Ordering::Relaxed) {
                probe_http::ProbeState::Draining
            } else if reg.snapshot().iter().any(|w| w.health == WorkerHealth::Alive) {
                probe_http::ProbeState::Ready
            } else {
                probe_http::ProbeState::NotReady
            }
        })
    };

    // Health sampler: publish the worker-health gauges + `sip_proxy_worker_pool_empty`
    // from the live registry so an empty/all-unprobed pool is observable in
    // Prometheus (not just via readiness). Cheap snapshot every probe interval.
    {
        let reg = registry.clone();
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            loop {
                ticker.tick().await;
                let (mut alive, mut draining, mut not_ready, mut unknown, mut dead) = (0, 0, 0, 0, 0);
                for w in reg.snapshot() {
                    match w.health {
                        WorkerHealth::Alive => alive += 1,
                        WorkerHealth::Draining => draining += 1,
                        WorkerHealth::NotReady => not_ready += 1,
                        WorkerHealth::Unknown => unknown += 1,
                        WorkerHealth::Dead => dead += 1,
                    }
                }
                m.set_worker_health_counts(alive, draining, not_ready, unknown, dead);
            }
        });
    }

    // Stale-payload sweep (port of bin/proxy.ts:336-350 `loadObserverSweepLayer`).
    // A 1 s `tokio::time::interval` (NOT the TS raw `Effect.sleep` loop) ticks the
    // observer's `sweep_stale(now_ms)`: any Alive worker whose OPTIONS replies stop
    // carrying a fresh `X-Overload` payload (or whose probe stalls) is conservatively
    // decreased once `payload_stale_ms` (8000) is exceeded, instead of keeping its
    // last AIMD cap forever while the LB admits at full rate into a worker it has
    // lost telemetry on. The probe cycle (interval 1000 + timeout 1500) is well
    // inside the stale window, so a HEALTHY worker is never floored — the decrease
    // only fires when telemetry actually dries up. Shares the probe's observer
    // `Arc`; owns no per-call state, so no release path. Each sweep's floored-worker
    // count feeds the coarse `stale_decrease` aggregate counter so a silently
    // floored cap is diagnosable in Prometheus (the per-worker push is a deferred
    // slice; see load_observer.rs TODO(metrics)).
    {
        let obs = sweep_observer;
        let clk = clock.clone();
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(1));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t.tick().await; // skip the immediate first tick (TS first fire is +1s)
            loop {
                t.tick().await;
                let floored = obs.sweep_stale(clk.now_ms());
                if floored > 0 {
                    m.record_overload_stale_decrease(floored);
                }
            }
        });
    }

    // Proxy-self gate sampler (migration/14). Rides `tokio::time::interval` (NOT
    // the TS raw `setInterval`) so behaviour stays on one clock; each tick reads
    // the ELU/GC load sampler and feeds the gate's EWMA. Only spawned when the
    // real gate is enabled. Owns no per-call state, so it needs no release path;
    // the live sampler's busy proxy keys on how late this task lands under load.
    if let Some(g) = self_gate.clone() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(g.sampler_interval());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // skip the immediate first tick (TS first fire is +interval)
            loop {
                tick.tick().await;
                g.sample();
            }
        });
    }

    // Prometheus + readiness endpoint (shared `probe-http`). NOTE: `ProbeServer`
    // aborts its accept loop on Drop, so the handle must stay live for the whole
    // process lifetime. The `/metrics` body appends the proxy-self gate gauges
    // (migration/14) + the jemalloc mallctl exposition (footprint/purge/decay
    // config) per scrape — the latter same cfg as the allocator dep.
    let metrics_gate = self_gate.clone();
    let routes = probe_http::ProbeRoutes {
        metrics: Arc::new(move || {
            let mut t = metrics.prometheus_text();
            t.push_str(&self_gate_prometheus_text(&metrics_gate));
            #[cfg(not(target_env = "msvc"))]
            t.push_str(&jemalloc_stats::prometheus_text());
            t
        }),
        ready,
        // No heap profiling on the proxy (not the leak target; profiling build
        // feature off here). /debug/heap → 503.
        heap: None,
    };
    let _metrics_server = match probe_http::ProbeServer::start(metrics_sa, routes).await {
        Ok(s) => {
            eprintln!("sip-proxy-runner metrics on http://{}/metrics (readiness /readyz)", s.addr());
            Some(s)
        }
        Err(e) => {
            eprintln!("sip-proxy-runner metrics server failed to bind {metrics_sa}: {e}");
            None
        }
    };

    let probe_task = tokio::spawn(probe.run());
    let mut core_tasks = tokio::task::JoinSet::new();
    for core in cores {
        core_tasks.spawn(core.run());
    }

    // Graceful shutdown on SIGTERM (k8s pod termination). The proxy holds no
    // per-call state to quiesce — it is a stateless forwarder — so the drain is
    // latch-then-grace: flip `/readyz` to Draining (k8s unpublishes the pod and
    // stops routing new datagrams here), then wait the grace so in-flight
    // transactions settle and the EndpointSlice withdrawal propagates before
    // exit. Ctrl-C (interactive) exits at once.
    let drain_grace_ms: u64 = env_or("PROXY_DRAIN_GRACE_MS", "5000").parse().unwrap_or(5000);

    // Supervise every data-path task: a panicked/exited recv loop used to
    // leave the process alive with /healthz green — k8s never restarted the
    // pod and every datagram was silently black-holed. Exiting non-zero makes
    // the container restart the moment ANY recv shard or the probe dies.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("sip-proxy-runner SIGINT — shutting down");
        }
        _ = wait_sigterm() => {
            eprintln!("sip-proxy-runner SIGTERM — draining ({drain_grace_ms}ms grace)");
            draining.store(true, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(drain_grace_ms)).await;
            eprintln!("sip-proxy-runner drain grace elapsed — exiting");
        }
        res = core_tasks.join_next() => {
            eprintln!("sip-proxy-runner FATAL: a SIP recv shard exited ({res:?}) — exiting for restart");
            std::process::exit(1);
        }
        res = probe_task => {
            eprintln!("sip-proxy-runner FATAL: health probe exited ({res:?}) — exiting for restart");
            std::process::exit(1);
        }
    }
    drop(_metrics_server);
}

/// Await a SIGTERM (k8s sends this on pod termination). On non-unix this future
/// never resolves (only Ctrl-C drives shutdown there).
#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            eprintln!("sip-proxy-runner cannot install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    //! Port of `tests/sip-front-proxy/config-validation.test.ts`.
    //!
    //! The validator is a pure boot-time guard (no clock, no `tokio`, no I/O), so
    //! these are plain `#[test]`s driven with literal config values exactly as the
    //! TS `vitest` suite is — no paused runtime needed (default lane, sub-ms).
    use super::*;

    /// The Helm chart's production-shape probe timings. Acts as the canonical
    /// "real deployment" fixture: cycle = 2000 + 1500 = 3500 ms. If a test breaks
    /// because the chart changes its defaults, that is the signal to revisit the
    /// calibration (mirrors the TS `HELM_PROBE`).
    const HELM_PROBE: ProbeTimingConfig = ProbeTimingConfig { interval_ms: 2000, timeout_ms: 1500 };

    /// `defaultWorkerLoadObserverConfig` + the given overrides (TS `withObserver`).
    fn with_observer(f: impl FnOnce(&mut LoadObserverConfig)) -> LoadObserverConfig {
        let mut cfg = LoadObserverConfig::default();
        f(&mut cfg);
        cfg
    }

    fn msgs(probe: ProbeTimingConfig, observer: &LoadObserverConfig) -> Vec<String> {
        validate_config(probe, observer).err().unwrap_or_default()
    }

    // ── happy path ─────────────────────────────────────────────────────────

    #[test]
    fn shipped_defaults_pass_against_the_deployed_helm_probe_timings() {
        assert!(validate_config(HELM_PROBE, &LoadObserverConfig::default()).is_ok());
    }

    #[test]
    fn happy_path_against_the_in_code_defaults_interval_1000() {
        let probe = ProbeTimingConfig { interval_ms: 1000, timeout_ms: 1500 };
        assert!(validate_config(probe, &LoadObserverConfig::default()).is_ok());
    }

    // ── primary invariant: payload_stale_ms vs probe cycle ──────────────────

    #[test]
    fn the_exact_rca_misconfiguration_is_rejected() {
        // Helm 3500 ms cycle, observer 3000 ms stale.
        let m = msgs(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000)).join("\n");
        assert!(m.contains("payload_stale_ms"), "{m}");
        assert!(m.contains("3500 ms"), "cycle reported: {m}");
        assert!(m.contains("7000 ms"), "2x cycle reported: {m}");
    }

    #[test]
    fn rejects_exactly_one_cycle_margin() {
        // probe_cycle = 3500, stale = 3500 → still rejected (need 2×).
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3500)).is_err());
    }

    #[test]
    fn accepts_exactly_2x_cycle_boundary() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 7000)).is_ok());
    }

    #[test]
    fn accepts_the_shipped_default_of_8000_ms() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 8000)).is_ok());
    }

    // ── value-positivity guards ─────────────────────────────────────────────
    // NOTE: the TS NaN / negative-interval / negative-timeout cases are
    // unrepresentable here — probe timings are `u64`, so only the `== 0` arm of
    // the TS positivity guard is reachable for interval/timeout. payload_stale_ms
    // is `i64`, so its negative case IS exercised below.

    #[test]
    fn rejects_interval_ms_zero() {
        let probe = ProbeTimingConfig { interval_ms: 0, timeout_ms: 1500 };
        assert!(validate_config(probe, &LoadObserverConfig::default()).is_err());
    }

    #[test]
    fn rejects_timeout_ms_zero() {
        let probe = ProbeTimingConfig { interval_ms: 2000, timeout_ms: 0 };
        assert!(validate_config(probe, &LoadObserverConfig::default()).is_err());
    }

    #[test]
    fn rejects_payload_stale_ms_zero() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 0)).is_err());
    }

    #[test]
    fn rejects_payload_stale_ms_negative() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = -1)).is_err());
    }

    // ── cap-band sanity ─────────────────────────────────────────────────────

    #[test]
    fn rejects_cap_floor_gt_cap_initial() {
        let m = msgs(
            HELM_PROBE,
            &with_observer(|c| {
                c.cap_floor_cps = 50.0;
                c.cap_initial_cps = 30.0;
            }),
        )
        .join("\n");
        assert!(m.contains("cap_floor_cps"), "{m}");
    }

    #[test]
    fn rejects_cap_initial_gt_cap_ceiling() {
        assert!(validate_config(
            HELM_PROBE,
            &with_observer(|c| {
                c.cap_initial_cps = 500.0;
                c.cap_ceiling_cps = 200.0;
            }),
        )
        .is_err());
    }

    #[test]
    fn rejects_cap_floor_zero() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.cap_floor_cps = 0.0)).is_err());
    }

    // ── ELU band ordering and hysteresis (delegates to validate_bands) ──────

    #[test]
    fn rejects_elu_soft_ge_elu_hard() {
        assert!(validate_config(
            HELM_PROBE,
            &with_observer(|c| {
                c.elu_soft = 0.6;
                c.elu_hard = 0.6;
            }),
        )
        .is_err());
    }

    #[test]
    fn rejects_elu_hard_ge_elu_critical() {
        assert!(validate_config(
            HELM_PROBE,
            &with_observer(|c| {
                c.elu_hard = 0.75;
                c.elu_critical = 0.75;
            }),
        )
        .is_err());
    }

    #[test]
    fn rejects_hysteresis_wider_than_a_band_gap() {
        // hard − soft = 0.6 − 0.4 = 0.2; hysteresis 0.25 traps the controller.
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = 0.25)).is_err());
    }

    #[test]
    fn rejects_negative_hysteresis() {
        assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = -0.01)).is_err());
    }

    // ── cooldown sanity ─────────────────────────────────────────────────────

    #[test]
    fn rejects_cooldown_shorter_than_one_probe_cycle() {
        // cooldown = 1 × 1000 = 1000 ms < cycle 3500 ms.
        let m = msgs(
            HELM_PROBE,
            &with_observer(|c| {
                c.aimd_cooldown_ticks = 1.0;
                c.options_interval_ms = 1000;
            }),
        )
        .join("\n");
        assert!(m.contains("cooldown"), "{m}");
    }

    #[test]
    fn accepts_cooldown_ge_one_probe_cycle() {
        // cooldown = 4 × 1000 = 4000 ms >= 3500 ms cycle.
        assert!(validate_config(
            HELM_PROBE,
            &with_observer(|c| {
                c.aimd_cooldown_ticks = 4.0;
                c.options_interval_ms = 1000;
            }),
        )
        .is_ok());
    }

    // ── multiple simultaneous violations are all reported ───────────────────

    #[test]
    fn invalid_probe_and_invalid_observer_both_surface() {
        let probe = ProbeTimingConfig { interval_ms: 0, timeout_ms: 1500 };
        let violations = msgs(
            probe,
            &with_observer(|c| {
                c.payload_stale_ms = 100;
                c.cap_floor_cps = 99.0;
                c.cap_initial_cps = 30.0;
            }),
        );
        // Operators must see ALL problems on the first boot attempt, not play
        // whack-a-mole one error at a time.
        assert!(violations.len() > 1, "expected >1 violation, got {violations:?}");
    }

    // ── assert_valid_config — panics on violation ───────────────────────────

    #[test]
    #[should_panic(expected = "refusing to start")]
    fn assert_valid_config_panics_with_the_refuse_to_start_header() {
        assert_valid_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000));
    }

    #[test]
    fn assert_valid_config_does_not_panic_on_a_valid_config() {
        // No `#[should_panic]` ⇒ a panic here fails the test.
        assert_valid_config(HELM_PROBE, &LoadObserverConfig::default());
    }

    // ── load_observer_cfg_from_env — LB_* operator overrides (migration/32) ──
    //
    // The process env is global, so these serialise behind one mutex and clean up
    // every var they touch. They never spawn a runtime — pure CPU, default lane.

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const LB_VARS: &[&str] = &[
        "LB_ELU_SOFT",
        "LB_ELU_HARD",
        "LB_ELU_CRITICAL",
        "LB_BAND_HYSTERESIS",
        "LB_AIMD_DECREASE_FACTOR",
        "LB_AIMD_INCREASE_STEP_CPS",
        "LB_CAP_CEILING_CPS",
        "LB_CAP_FLOOR_CPS",
    ];

    fn clear_lb_vars() {
        for k in LB_VARS {
            env::remove_var(k);
        }
    }

    /// With no `LB_*` vars set, the config is the shipped default verbatim.
    #[test]
    fn env_overrides_absent_keep_the_shipped_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_lb_vars();
        let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
        let def = LoadObserverConfig::default();
        assert_eq!(cfg.elu_soft, def.elu_soft);
        assert_eq!(cfg.elu_hard, def.elu_hard);
        assert_eq!(cfg.elu_critical, def.elu_critical);
        assert_eq!(cfg.cap_ceiling_cps, def.cap_ceiling_cps);
    }

    /// Each present, parseable `LB_*` var replaces exactly its field; a coherent
    /// override set still passes the boot preflight.
    #[test]
    fn env_overrides_present_replace_fields_and_stay_coherent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_lb_vars();
        env::set_var("LB_ELU_SOFT", "0.3");
        env::set_var("LB_ELU_HARD", "0.45");
        env::set_var("LB_ELU_CRITICAL", "0.6");
        env::set_var("LB_BAND_HYSTERESIS", "0.05");
        env::set_var("LB_AIMD_DECREASE_FACTOR", "0.4");
        env::set_var("LB_CAP_CEILING_CPS", "150");
        env::set_var("LB_CAP_FLOOR_CPS", "2");
        let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
        clear_lb_vars();
        assert_eq!(cfg.elu_soft, 0.3);
        assert_eq!(cfg.elu_hard, 0.45);
        assert_eq!(cfg.elu_critical, 0.6);
        assert_eq!(cfg.aimd_decrease_factor, 0.4);
        assert_eq!(cfg.cap_ceiling_cps, 150.0);
        assert_eq!(cfg.cap_floor_cps, 2.0);
        // The result is coherent → the preflight accepts it.
        assert_valid_config(HELM_PROBE, &cfg);
    }

    /// A malformed value is IGNORED (the default field is kept) — never aborts.
    #[test]
    fn env_overrides_malformed_value_is_ignored() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_lb_vars();
        env::set_var("LB_ELU_CRITICAL", "not-a-number");
        let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
        clear_lb_vars();
        assert_eq!(cfg.elu_critical, LoadObserverConfig::default().elu_critical);
    }

    /// An INCOHERENT override set (reversed bands) is NOT silently accepted: it
    /// applies, then the boot preflight refuses to start.
    #[test]
    #[should_panic(expected = "refusing to start")]
    fn env_overrides_incoherent_set_fails_the_preflight() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_lb_vars();
        // elu_critical < elu_hard < elu_soft — fully reversed band ordering.
        env::set_var("LB_ELU_SOFT", "0.8");
        env::set_var("LB_ELU_HARD", "0.5");
        env::set_var("LB_ELU_CRITICAL", "0.3");
        let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
        clear_lb_vars();
        assert_valid_config(HELM_PROBE, &cfg); // panics: refusing to start
    }
}
