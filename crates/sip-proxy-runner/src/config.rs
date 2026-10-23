//! [`ProxyConfig`] — every variable the proxy reads at boot, read once
//! through a lookup ([`ProxyConfig::from_lookup`]). The keys, their defaults
//! and their ranges are listed in the `sip-proxy-runner` binary's module doc.

mod preflight;
mod process_env;
mod reader;

use std::net::SocketAddr;
use std::time::Duration;

use sip_proxy::health::HealthProbeConfig;
use sip_proxy::load_observer::LoadObserverConfig;
use sip_proxy::resolver::ResolverConfig;
use sip_proxy::self_gate::ProxySelfGateConfig;
use sip_proxy::{FaceCidrs, ProxyAddr};

pub use preflight::{refusal, validate_config, ProbeTimingConfig};
use reader::{EnvReader, Span};

/// Where the worker pool comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerSource {
    /// `PROXY_WORKERS`: a fixed `id@host:port,..` list.
    Static(String),
    /// The EndpointSlices of `service` in `namespace`, each worker reached at
    /// its pod IP on `port`.
    Discovered { service: String, namespace: String, port: u16 },
}

/// The external (caller-facing) face of a dual-face proxy.
#[derive(Debug, Clone)]
pub struct ExternalFace {
    pub listen: SocketAddr,
    pub advertise: ProxyAddr,
    pub int_cidrs: FaceCidrs,
}

/// The proxy's boot configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub advertise: ProxyAddr,
    pub external: Option<ExternalFace>,
    pub workers: WorkerSource,
    pub metrics: SocketAddr,
    pub queue_max: usize,
    pub udp_sndbuf: Option<usize>,
    pub udp_rcvbuf: Option<usize>,
    pub shard_stall_ms: u64,
    pub recv_shards: usize,
    pub bind_retry_interval: Duration,
    pub bind_retry_deadline: Duration,
    pub hmac_kid: String,
    pub hmac_key: String,
    pub probe: HealthProbeConfig,
    pub observer: LoadObserverConfig,
    /// The intake-shed watermark, percent of `queue_max`.
    pub intake_shed_pct: u64,
    /// `None`: the always-admit gate.
    pub self_gate: Option<ProxySelfGateConfig>,
    /// The spread, seconds, added to a refusal's `Retry-After` base.
    pub retry_after_jitter_sec: u32,
    pub resolver: ResolverConfig,
    pub prewarm: Vec<ProxyAddr>,
    pub drain_grace: Duration,
}

/// The largest millisecond value a duration variable takes (about 24 days).
const MAX_MS: u64 = u32::MAX as u64;

impl ProxyConfig {
    /// Reads every boot variable through `get` (`None` = unset).
    /// `pod_namespace` reads the namespace of the pod the proxy runs in.
    /// `Err` holds every problem, one line each.
    pub fn from_lookup(
        get: &dyn Fn(&str) -> Option<String>,
        pod_namespace: &dyn Fn() -> Option<String>,
    ) -> Result<Self, Vec<String>> {
        let mut env = EnvReader::new(get);

        let listen = env.bind_addr("PROXY_LISTEN", SocketAddr::from(([0, 0, 0, 0], 5060)));
        let advertise = advertise(&mut env, "PROXY_ADVERTISE", listen);
        let external = external_face(&mut env, listen);
        let listen = listen.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 5060)));
        let workers = worker_source(&mut env, pod_namespace);
        let metrics = env
            .bind_addr("PROXY_METRICS", SocketAddr::from(([0, 0, 0, 0], 9090)))
            .unwrap_or(SocketAddr::from(([0, 0, 0, 0], 9090)));
        let queue_max = env.int_usize("PROXY_QUEUE", 8192, 1, u32::MAX);
        let udp_sndbuf = socket_buffer(&mut env, "PROXY_UDP_SNDBUF");
        let udp_rcvbuf = socket_buffer(&mut env, "PROXY_UDP_RCVBUF");
        let shard_stall_ms = env.int("PROXY_SHARD_STALL_MS", 2000, 1, MAX_MS);
        let recv_shards = env.int_usize("PROXY_RECV_SHARDS", 1, 1, 1024);
        let bind_retry_interval =
            Duration::from_millis(env.int("PROXY_BIND_RETRY_INTERVAL_MS", 200, 1, MAX_MS));
        let bind_retry_deadline =
            Duration::from_millis(env.int("PROXY_BIND_RETRY_DEADLINE_MS", 10_000, 1, MAX_MS));
        let hmac_kid = env.text("PROXY_HMAC_KID", "k0");
        let hmac_key = hmac_key(&mut env);
        let probe = HealthProbeConfig {
            interval_ms: env.int("HEALTH_INTERVAL_MS", 1000, 1, MAX_MS),
            timeout_ms: env.int("HEALTH_TIMEOUT_MS", 1500, 1, MAX_MS),
            threshold: env.int_u32("HEALTH_THRESHOLD", 2, 1, u32::MAX),
        };
        let observer = observer_overrides(&mut env, LoadObserverConfig::default());
        let intake_shed_pct = env.int("PROXY_INTAKE_SHED_PCT", 50, 0, 100);
        let self_gate = self_gate(&mut env);
        let retry_after_jitter_sec = env.int_u32("PROXY_RETRY_AFTER_JITTER_SEC", 5, 0, 3600);
        let resolver = resolver(&mut env);
        let prewarm = prewarm(&mut env);
        let drain_grace = Duration::from_millis(env.int("PROXY_DRAIN_GRACE_MS", 5000, 1, MAX_MS));

        let mut errors = env.into_errors();
        let timings =
            ProbeTimingConfig { interval_ms: probe.interval_ms, timeout_ms: probe.timeout_ms };
        if let Err(violations) = validate_config(timings, &observer) {
            errors.extend(violations);
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            listen,
            advertise,
            external,
            workers,
            metrics,
            queue_max,
            udp_sndbuf,
            udp_rcvbuf,
            shard_stall_ms,
            recv_shards,
            bind_retry_interval,
            bind_retry_deadline,
            hmac_kid,
            hmac_key,
            probe,
            observer,
            intake_shed_pct,
            self_gate,
            retry_after_jitter_sec,
            resolver,
            prewarm,
            drain_grace,
        })
    }
}

/// The address stamped on Via and Record-Route for egress on the face that
/// listens on `listen`: `key`, else the listen address. An unspecified listen
/// address names no reachable host, so `key` is then required. A refused
/// listen address (`None`) asks nothing more of `key`.
fn advertise(env: &mut EnvReader, key: &str, listen: Option<SocketAddr>) -> ProxyAddr {
    let Some(listen) = listen else {
        return ProxyAddr::new("0.0.0.0", 0);
    };
    let fallback = ProxyAddr::new(listen.ip().to_string(), listen.port());
    match env.present(key) {
        Some(s) => ProxyAddr::parse(&s).unwrap_or_else(|| {
            env.refuse(format!("{key} must be host:port, got {s:?}"));
            fallback
        }),
        None if listen.ip().is_unspecified() => {
            if env.raw(key).is_none() {
                env.refuse(format!(
                    "{key} is required: the listen address {listen} names no host to advertise"
                ));
            }
            fallback
        }
        None => fallback,
    }
}

/// `PROXY_LISTEN_EXT` enables the external face; it then needs
/// `PROXY_FACE_INT_CIDRS`, and each face binds its own address: a wildcard
/// bind of `PROXY_LISTEN` (the internal face, `listen`) or of
/// `PROXY_LISTEN_EXT` would let the external plane reach the trusted internal
/// socket (ADR-0048).
fn external_face(env: &mut EnvReader, listen: Option<SocketAddr>) -> Option<ExternalFace> {
    let listen_ext = env.present("PROXY_LISTEN_EXT");
    let cidrs = env.present("PROXY_FACE_INT_CIDRS");
    let int_cidrs = match parse_ext_face_cidrs(listen_ext.is_some(), cidrs.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            env.refuse(e);
            return None;
        }
    };
    if listen_ext.is_none() && cidrs.is_some() {
        tracing::warn!(
            "PROXY_FACE_INT_CIDRS set but PROXY_LISTEN_EXT unset — single-face mode, CIDR list ignored"
        );
    }
    if listen_ext.is_none() {
        tracing::warn!(
            "single-face proxy (PROXY_LISTEN_EXT unset): every sender is trusted as the internal \
             plane and may have a request relayed to its Request-URI (ADR-0048)"
        );
    }
    listen_ext?;
    let int_listen = listen;
    let listen = env.bind_addr("PROXY_LISTEN_EXT", SocketAddr::from(([0, 0, 0, 0], 0)));
    let advertise = advertise(env, "PROXY_ADVERTISE_EXT", listen);
    for (key, addr) in [("PROXY_LISTEN", int_listen), ("PROXY_LISTEN_EXT", listen)] {
        if let Some(addr) = addr.filter(|a| a.ip().is_unspecified()) {
            env.refuse(format!(
                "{key} {addr} is a wildcard: a dual-face proxy binds each face to its own address"
            ));
        }
    }
    let listen = listen.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
    Some(ExternalFace { listen, advertise, int_cidrs: int_cidrs? })
}

/// Dual-face boot rule: `PROXY_FACE_INT_CIDRS` is required, and must parse,
/// whenever `PROXY_LISTEN_EXT` enables the second face — a dual-face proxy
/// without a plane classifier would egress worker traffic on the caller plane.
/// A single-face proxy never reads it.
pub fn parse_ext_face_cidrs(
    listen_ext_set: bool,
    cidrs: Option<&str>,
) -> Result<Option<FaceCidrs>, String> {
    if !listen_ext_set {
        return Ok(None);
    }
    match cidrs.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            Err("PROXY_FACE_INT_CIDRS is REQUIRED when PROXY_LISTEN_EXT is set: the dual-face \
             egress picker needs the internal-plane CIDR list (e.g. \
             PROXY_FACE_INT_CIDRS=10.244.0.0/16,172.20.0.0/16)."
                .to_string())
        }
        Some(s) => {
            FaceCidrs::parse(s).map(Some).map_err(|e| format!("bad PROXY_FACE_INT_CIDRS: {e}"))
        }
    }
}

/// `PROXY_WORKERS` when set, else EndpointSlice discovery in
/// `PROXY_NAMESPACE`, else `POD_NAMESPACE`, else the pod's own namespace.
/// The discovery variables are checked in both modes.
fn worker_source(env: &mut EnvReader, pod_namespace: &dyn Fn() -> Option<String>) -> WorkerSource {
    let list = env.present("PROXY_WORKERS");
    let service = env.text("PROXY_WORKER_SERVICE", "b2bua-worker");
    let port = env.int_u16("PROXY_WORKER_PORT", 5060, 1, u16::MAX);
    let named = match env.present("PROXY_NAMESPACE") {
        Some(ns) => Some(ns),
        None => env.present("POD_NAMESPACE"),
    };
    if let Some(list) = list {
        return WorkerSource::Static(list);
    }
    let namespace = named
        .or_else(|| pod_namespace().map(|ns| ns.trim().to_string()).filter(|ns| !ns.is_empty()))
        .unwrap_or_else(|| {
            env.refuse(
                "the worker namespace is unknown: set PROXY_NAMESPACE (or POD_NAMESPACE), \
                 or PROXY_WORKERS for a fixed pool"
                    .to_string(),
            );
            String::new()
        });
    WorkerSource::Discovered { service, namespace, port }
}

/// `PROXY_HMAC_KEY`, the secret that signs the stickiness cookies: required,
/// trimmed like every value, at least 16 bytes.
fn hmac_key(env: &mut EnvReader) -> String {
    let unset = env.raw("PROXY_HMAC_KEY").is_none();
    match env.present("PROXY_HMAC_KEY") {
        Some(key) if key.len() >= 16 => key,
        Some(_) => {
            env.refuse("PROXY_HMAC_KEY must be at least 16 bytes".to_string());
            String::new()
        }
        None => {
            if unset {
                env.refuse(
                    "PROXY_HMAC_KEY is required: the secret that signs the stickiness cookies"
                        .into(),
                );
            }
            String::new()
        }
    }
}

/// A `PROXY_UDP_*BUF` request in bytes: 4 MiB when unset, the kernel default
/// (`None`) when set empty.
fn socket_buffer(env: &mut EnvReader, key: &str) -> Option<usize> {
    match env.raw(key) {
        Some(v) if v.trim().is_empty() => None,
        _ => Some(env.int_usize(key, 4 * 1024 * 1024, 1, u32::MAX)),
    }
}

/// The `LB_*` band, AIMD and cap overrides onto `cfg`.
fn observer_overrides(env: &mut EnvReader, mut cfg: LoadObserverConfig) -> LoadObserverConfig {
    let mut set = |key: &str, field: &mut f64, span: Span| *field = env.real(key, *field, span);
    set("LB_ELU_SOFT", &mut cfg.elu_soft, Span::Fraction);
    set("LB_ELU_HARD", &mut cfg.elu_hard, Span::Fraction);
    set("LB_ELU_CRITICAL", &mut cfg.elu_critical, Span::Fraction);
    set("LB_BAND_HYSTERESIS", &mut cfg.band_hysteresis, Span::FromZero);
    set("LB_AIMD_DECREASE_FACTOR", &mut cfg.aimd_decrease_factor, Span::OpenUnit);
    set("LB_AIMD_INCREASE_STEP_CPS", &mut cfg.aimd_increase_step_cps, Span::Positive);
    set("LB_CAP_CEILING_CPS", &mut cfg.cap_ceiling_cps, Span::Positive);
    set("LB_CAP_FLOOR_CPS", &mut cfg.cap_floor_cps, Span::Positive);
    cfg
}

/// The proxy-self gate, `None` when `PROXY_SELF_GATE` turns it off.
fn self_gate(env: &mut EnvReader) -> Option<ProxySelfGateConfig> {
    let on = env.flag("PROXY_SELF_GATE", true);
    let d = ProxySelfGateConfig::default();
    let cfg = ProxySelfGateConfig {
        elu_critical: env.real("PROXY_SELF_GATE_ELU_CRITICAL", d.elu_critical, Span::Fraction),
        cps_bucket_size: env.int_u32("PROXY_SELF_GATE_CPS_SIZE", d.cps_bucket_size, 1, u32::MAX),
        cps_bucket_rate: env.int_u32("PROXY_SELF_GATE_CPS_RATE", d.cps_bucket_rate, 1, u32::MAX),
        ..d
    };
    on.then_some(cfg)
}

/// The named-target resolver's cache and lookup bounds.
fn resolver(env: &mut EnvReader) -> ResolverConfig {
    let d = ResolverConfig::default();
    ResolverConfig {
        positive_ttl_ms: env.int("PROXY_RESOLVER_POSITIVE_TTL_MS", d.positive_ttl_ms, 1, MAX_MS),
        negative_ttl_ms: env.int("PROXY_RESOLVER_NEGATIVE_TTL_MS", d.negative_ttl_ms, 1, MAX_MS),
        lookup_timeout_ms: env.int(
            "PROXY_RESOLVER_LOOKUP_TIMEOUT_MS",
            d.lookup_timeout_ms,
            1,
            MAX_MS,
        ),
        ..d
    }
}

/// `PROXY_RESOLVER_PREWARM`: comma-separated `host:port` targets.
fn prewarm(env: &mut EnvReader) -> Vec<ProxyAddr> {
    let raw = env.present("PROXY_RESOLVER_PREWARM").unwrap_or_default();
    let mut targets = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match ProxyAddr::parse(entry) {
            Some(t) => targets.push(t),
            None => env.refuse(format!("PROXY_RESOLVER_PREWARM entry {entry:?} is not host:port")),
        }
    }
    targets
}

#[cfg(test)]
mod tests;
