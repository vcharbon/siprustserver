//! Reusable B2BUA runner composition: everything between env vars and a
//! running [`B2buaCore`], shared by every runner binary.
//!
//! Per ADR-0016 a downstream integrator composes callflow services and injects
//! its own [`CallDecisionEngine`] in a runner binary it owns. The composition
//! *seam* (`B2buaCore::spawn_with_services`, [`B2buaDeps`], [`B2buaConfig`])
//! reuses cleanly — but everything between env vars and a populated
//! [`B2buaDeps`] used to be inline-private in each runner's `main.rs`, so every
//! downstream runner copied it verbatim and silently diverged as upstream
//! policy (env grammar, advertise rules, overload knobs, drain behavior)
//! evolved. This crate is that plumbing, published once:
//!
//! - [`RunnerEnv::from_env`] — the `B2BUA_*`/`LIMITER_*`/`WORKER_*` env
//!   grammar parsed into a plain struct of pub fields (tweak any before bind);
//! - [`RunnerEnv::bind`] — UDP bind with the Tier-1 overload brake installed,
//!   advertise-address coercion, [`B2buaConfig`] assembly + validation;
//! - [`RunnerBase::deps`] — production-shaped defaults for every dependency
//!   (store / buffered CDR / limiter-from-env / metrics / clock / id-gen),
//!   each overridable field-by-field on the returned [`B2buaDeps`];
//! - [`RunnerBase::spawn`] / [`RunnerBase::spawn_probe_server`] /
//!   [`RunnerBase::spawn_gauge_sampler`] / [`RunnerBase::run_until_shutdown`]
//!   — core spawn, the `/metrics`+`/ready` probe, the memory-attribution
//!   sampler, and SIGTERM-drain handling;
//! - the shared helpers ([`env_or`], [`env_flag`], [`resolve`],
//!   [`split_host_port`], [`NullCdrWriter`]) previously private per binary.
//!
//! The in-tree `b2bua-runner` is the first consumer, so the surface provably
//! stays sufficient to build a full production worker. A minimal downstream
//! runner is the staged skeleton:
//!
//! ```no_run
//! # async fn run(my_engine: std::sync::Arc<dyn b2bua::decision::CallDecisionEngine>) {
//! let base = b2bua_runner_kit::RunnerEnv::from_env().bind("my-runner").await;
//! let deps = base.deps(my_engine, None); // swap any B2buaDeps field before spawn
//! let core = base.spawn(deps, Vec::new()); // + your composed ServiceDefs
//! let _probe = base.spawn_probe_server(&core, None, None).await;
//! base.spawn_gauge_sampler(&core);
//! base.run_until_shutdown(&core).await;
//! # }
//! ```
//!
//! What deliberately does NOT live here: the decision engine (the one piece a
//! runner exists to choose), the composed service list, replication membership
//! discovery (kube-coupled; build a `ReplicationSetup` and assign
//! `deps.replication`), and binary-specific CDR sinks / allocator wiring —
//! those are the runner's own, injected through the seams above.

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::CallDecisionEngine;
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::{B2buaMetrics, UdpTransportMetrics};
use b2bua::rules::ServiceDef;
use b2bua::store::InMemoryCallStore;
use b2bua::target_admission::{classify_admission, AdmissionVerdict};
use b2bua::tier1_brake::{
    build_tier1_brake_hook, entropy_roll, Tier1BrakeConfig, Tier1BrakeCounters,
};
use b2bua::{B2buaCore, B2buaDeps};
use call::Call;
use http_net::RealHttpNetwork;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use sip_txn::IdGen;

/// A CDR sink that discards every record. The default sink when a runner wires
/// no external CDR store — for load/endurance the process must not accumulate
/// records in memory. Always wrapped by `BufferedCdrWriter` (see
/// [`RunnerBase::deps`]) so the buffer/drainer machinery is still exercised.
pub struct NullCdrWriter;

#[async_trait]
impl CdrWriter for NullCdrWriter {
    async fn write(&self, _call: &Call, _terminated_at: i64) {}
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
}

/// Env var with a default when unset.
pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Truthy env flag: `1`/`true`/`yes`/`on` (case-insensitive) → true.
pub fn env_flag(key: &str) -> bool {
    is_truthy(&env_or(key, "0"))
}

fn is_truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Resolve a `host:port` string to a socket address, panicking with a clear
/// boot-refusal message when it cannot be resolved.
pub fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| panic!("cannot resolve {addr:?}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("no address resolved from {addr:?}"))
}

/// Split a `host:port` into its parts WITHOUT DNS resolution (the host may be a
/// service name resolved per-call downstream). Port defaults to 5060 if absent
/// or unparseable. Used for a b-leg callee default (`B2BUA_DEST`-style knobs) —
/// resolving at boot would pin every call to one startup-resolved pod.
pub fn split_host_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5060)),
        None => (s.to_string(), 5060),
    }
}

/// The Tier-1 brake percentage must keep the brake meaningful: it fires at
/// ingress depth >= queue_max × pct/100, so 0 would shed every packet and >100
/// would never fire. Checked at [`RunnerEnv::bind`]; exported for runners that
/// override the field and want the same boot refusal.
pub fn validate_tier1_pct(udp_tier1_pct: u32) -> Result<(), String> {
    if !(1..=100).contains(&udp_tier1_pct) {
        return Err(format!(
            "B2BUA_UDP_TIER1_PCT={udp_tier1_pct} out of range 1..=100: the Tier-1 \
             brake fires at ingress depth >= queue_max × pct/100, so 0 would shed \
             every packet and >100 would never fire. Use 1..=100 (100 = brake only \
             when the queue is full)"
        ));
    }
    Ok(())
}

/// Boot-time coherence check for runners with a **static default callee**: the
/// b-leg admission gate (`apply_route`) classifies the callee's
/// `destination.host`, so a default the worker's own TargetAdmission allow-list
/// rejects means EVERY default-routed call is 503'd before the b-leg and the
/// worker can never serve one. Per-call destinations are runtime and not
/// checkable here, but a self-rejecting default is an unambiguous
/// misconfiguration to refuse boot on. Pure (no env / IO) so it is
/// unit-testable.
pub fn validate_default_dest(
    dest_host: &str,
    worker_allowed_target_suffixes: &[String],
) -> Result<(), String> {
    if classify_admission(dest_host, worker_allowed_target_suffixes) == AdmissionVerdict::Reject {
        return Err(format!(
            "B2BUA_DEST host {dest_host:?} is rejected by its own TargetAdmission \
             allow-list WORKER_ALLOWED_TARGET_SUFFIXES={worker_allowed_target_suffixes:?}: \
             every default-routed call would be 503'd before the b-leg. Add a \
             matching suffix (e.g. \".svc.cluster.local\"), use an IP literal, or \
             set \"*\" to disable the gate"
        ));
    }
    Ok(())
}

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
    /// `B2BUA_OUTBOUND_PROXY` — front-proxy `host:port` every b-leg
    /// (worker→callee) request is forced through (preloaded `Route ;lr;outbound`).
    /// REQUIRED in the k8s cluster (pod IPs are not peer-routable); unset →
    /// b-leg goes straight to the callee (local/dev only). A malformed value is
    /// fatal at parse — a silent fallback to pod-direct is exactly the
    /// endurance bug this prevents.
    pub outbound_proxy: Option<(String, u16)>,
    /// `B2BUA_METRICS` — Prometheus HTTP listen addr (default `0.0.0.0:9091`).
    pub metrics_addr: String,
    /// `B2BUA_QUEUE` — inbound UDP queue depth, packets (default 8192).
    pub queue_max: usize,
    /// `B2BUA_CDR_QUEUE` — buffered-CDR submit queue depth (default 1024).
    pub cdr_queue: usize,
    /// `B2BUA_ORDINAL` — worker ordinal stamped in callRef (default `w0`).
    pub ordinal: String,
    /// `B2BUA_CONCURRENCY` — handler concurrency ceiling (default 8192; a
    /// back-pressure SAFETY limit, not a rate governor).
    pub concurrency: usize,
    /// `B2BUA_CALL_CAP` — max concurrent calls before drop (default 1_000_000).
    pub call_cap: usize,
    /// `B2BUA_MAX_MESSAGES_PER_CALL` — loop/runaway cap-defense (default 200:
    /// TS default 100 + headroom for a multi-hour keepalive-held call).
    pub max_messages_per_call: u64,
    /// `B2BUA_KEEPALIVE_SEC` — in-dialog OPTIONS keepalive interval (default
    /// 300 s; a shorter poke breaks long-hold endurance traffic).
    pub keepalive_sec: i64,
    /// `B2BUA_KEEPALIVE_TIMEOUT_SEC` — grace waiting for the OPTIONS 200 before
    /// declaring the leg dead and BYE-ing (default 32 s so a reclaimed dialog's
    /// keepalive can round-trip across the post-reboot recovery window).
    pub keepalive_timeout_sec: i64,
    /// `B2BUA_REBOOT_BUDGET_SEC` — replicated-backup TTL / reboot budget
    /// (default 600; `config.validate()` forces it to outlast the keepalive).
    pub reboot_budget_sec: i64,
    /// `B2BUA_SETUP_TIMEOUT_SEC` — a-leg total setup deadline, reroutes
    /// included (default 150, below the 158 s txn backstop; <= 0 disables).
    pub setup_timeout_sec: i64,
    /// `B2BUA_CALL_CONTROL_TIMEOUT_MS` — decision-backend deadline per
    /// round-trip (default 5000; <= 0 disables — ADR-0022).
    pub call_control_timeout_ms: i64,
    /// `B2BUA_ACK_TIMEOUT_SEC` — 2xx-without-ACK give-up window (RFC 3261
    /// §13.3.1.4, 64·T1 = 32 s; <= 0 disables).
    pub ack_timeout_sec: i64,
    /// `B2BUA_CPS_BUCKET_SIZE` — Tier-3 admission gate bucket size (default 1000).
    pub cps_bucket_size: u32,
    /// `B2BUA_CPS_BUCKET_RATE` — Tier-3 admission gate refill rate (default 500).
    pub cps_bucket_rate: u32,
    /// `B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD` — panic-ELU backstop (default 0.75).
    pub overload_panic_elu_threshold: f64,
    /// `B2BUA_RETRY_AFTER_BASE_SEC` — base Retry-After on overload 503s (default 5).
    pub retry_after_base_sec: u32,
    /// `WORKER_ALLOWED_TARGET_SUFFIXES` — b-leg target-admission allow-list,
    /// comma-separated (default `.svc.cluster.local`; `*` = allow all, the
    /// rollback sentinel; non-IP non-matching hosts are 503'd pre-leg).
    pub worker_allowed_target_suffixes: Vec<String>,
    /// `B2BUA_UDP_TIER1_PCT` — Tier-1 ingress brake threshold percentage
    /// (default 70): at inbound-queue depth >= floor(queue_max × pct/100) a
    /// new, non-emergency INVITE is shed with a STATELESS 503 before the parser
    /// runs — the cheapest shed in the stack, ahead of the Tier-3 gate.
    pub udp_tier1_pct: u32,
    /// `B2BUA_RETRY_AFTER_JITTER_SEC` — the brake 503's Retry-After is
    /// `retry_after_base_sec + U[0, jitter]` (default 5).
    pub retry_after_jitter_sec: u32,
    /// `B2BUA_RELAY_HEADERS` — opt-in transparent header relay, comma-separated
    /// names copied from the a-leg INVITE onto every originated b-leg INVITE
    /// (default empty = no relay; structural headers never relayable).
    pub relay_headers: Vec<String>,
    /// `LIMITER_URL` — shared limiter base URL; empty → `NoopLimiter` (fail-open).
    pub limiter_url: String,
    /// `LIMITER_TIMEOUT_MS` — per-request fail-open budget (default 150).
    pub limiter_timeout_ms: u64,
    /// `LIMITER_WINDOW_SECONDS` — refresh cadence; MUST match the limiter
    /// service window (default 300).
    pub limiter_refresh_sec: i64,
    /// `B2BUA_DRAIN_GRACE_MS` — SIGTERM drain grace before exit (default 5000).
    pub drain_grace_ms: u64,
}

impl RunnerEnv {
    /// Parse the full generic env grammar. Panics (refuses boot) on an
    /// unparseable value — a typo'd knob must never silently become a default.
    pub fn from_env() -> Self {
        Self {
            listen: env_or("B2BUA_LISTEN", "0.0.0.0:5060"),
            advertise: env::var("B2BUA_ADVERTISE").ok(),
            outbound_proxy: match env::var("B2BUA_OUTBOUND_PROXY") {
                Ok(s) if !s.trim().is_empty() => {
                    let s = s.trim();
                    let (h, p) = s
                        .rsplit_once(':')
                        .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h.to_string(), p)))
                        .unwrap_or_else(|| {
                            panic!("B2BUA_OUTBOUND_PROXY must be host:port, got {s:?}")
                        });
                    Some((h, p))
                }
                _ => None,
            },
            metrics_addr: env_or("B2BUA_METRICS", "0.0.0.0:9091"),
            queue_max: env_or("B2BUA_QUEUE", "8192").parse().expect("B2BUA_QUEUE"),
            cdr_queue: env_or("B2BUA_CDR_QUEUE", "1024").parse().expect("B2BUA_CDR_QUEUE"),
            ordinal: env_or("B2BUA_ORDINAL", "w0"),
            // Dispatch throttle ceilings — deliberately high so they never cap
            // throughput below the offered rate; `cap_drops`/`saturation`
            // metrics flag if either is actually hit.
            concurrency: env_or("B2BUA_CONCURRENCY", "8192").parse().expect("B2BUA_CONCURRENCY"),
            call_cap: env_or("B2BUA_CALL_CAP", "1000000").parse().expect("B2BUA_CALL_CAP"),
            max_messages_per_call: env_or("B2BUA_MAX_MESSAGES_PER_CALL", "200")
                .parse()
                .expect("B2BUA_MAX_MESSAGES_PER_CALL"),
            keepalive_sec: env_or("B2BUA_KEEPALIVE_SEC", "300")
                .parse()
                .expect("B2BUA_KEEPALIVE_SEC"),
            keepalive_timeout_sec: env_or("B2BUA_KEEPALIVE_TIMEOUT_SEC", "32")
                .parse()
                .expect("B2BUA_KEEPALIVE_TIMEOUT_SEC"),
            reboot_budget_sec: env_or("B2BUA_REBOOT_BUDGET_SEC", "600")
                .parse()
                .expect("B2BUA_REBOOT_BUDGET_SEC"),
            setup_timeout_sec: env_or("B2BUA_SETUP_TIMEOUT_SEC", "150")
                .parse()
                .expect("B2BUA_SETUP_TIMEOUT_SEC"),
            call_control_timeout_ms: env_or("B2BUA_CALL_CONTROL_TIMEOUT_MS", "5000")
                .parse()
                .expect("B2BUA_CALL_CONTROL_TIMEOUT_MS"),
            ack_timeout_sec: env_or("B2BUA_ACK_TIMEOUT_SEC", "32")
                .parse()
                .expect("B2BUA_ACK_TIMEOUT_SEC"),
            cps_bucket_size: env_or("B2BUA_CPS_BUCKET_SIZE", "1000")
                .parse()
                .expect("B2BUA_CPS_BUCKET_SIZE"),
            cps_bucket_rate: env_or("B2BUA_CPS_BUCKET_RATE", "500")
                .parse()
                .expect("B2BUA_CPS_BUCKET_RATE"),
            overload_panic_elu_threshold: env_or("B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD", "0.75")
                .parse()
                .expect("B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD"),
            retry_after_base_sec: env_or("B2BUA_RETRY_AFTER_BASE_SEC", "5")
                .parse()
                .expect("B2BUA_RETRY_AFTER_BASE_SEC"),
            worker_allowed_target_suffixes: split_csv(&env_or(
                "WORKER_ALLOWED_TARGET_SUFFIXES",
                ".svc.cluster.local",
            )),
            udp_tier1_pct: env_or("B2BUA_UDP_TIER1_PCT", "70")
                .parse()
                .expect("B2BUA_UDP_TIER1_PCT"),
            retry_after_jitter_sec: env_or("B2BUA_RETRY_AFTER_JITTER_SEC", "5")
                .parse()
                .expect("B2BUA_RETRY_AFTER_JITTER_SEC"),
            relay_headers: split_csv(&env_or("B2BUA_RELAY_HEADERS", "")),
            limiter_url: env_or("LIMITER_URL", ""),
            limiter_timeout_ms: env_or("LIMITER_TIMEOUT_MS", "150").parse().unwrap_or(150),
            limiter_refresh_sec: env_or("LIMITER_WINDOW_SECONDS", "300").parse().unwrap_or(300),
            drain_grace_ms: env_or("B2BUA_DRAIN_GRACE_MS", "5000").parse().unwrap_or(5000),
        }
    }

    /// Bind the real UDP endpoint (Tier-1 brake installed), coerce the
    /// advertise address, assemble + validate the [`B2buaConfig`], and mint the
    /// process-wide metrics registry / system clock. Panics (refuses boot) on a
    /// bind failure or an invalid config. `name` prefixes every log line.
    pub async fn bind(self, name: &str) -> RunnerBase {
        validate_tier1_pct(self.udp_tier1_pct)
            .unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));

        let listen_sa = resolve(&self.listen);
        let metrics_sa = resolve(&self.metrics_addr);

        // Tier-1 overload brake: the `preIngress` hook + its counters, installed
        // on the worker socket. Without it the brake (the cheapest stateless-503
        // shed, ahead of Tier-3) is absent and a flooded ingress queue tail-drops
        // new INVITEs silently instead of returning a routable 503 + Retry-After.
        // The counters are retained for the `/metrics` scrape.
        let brake_counters = Tier1BrakeCounters::new();
        let brake_hook = build_tier1_brake_hook(
            Tier1BrakeConfig {
                queue_max: self.queue_max,
                tier1_threshold_pct: self.udp_tier1_pct,
                retry_after_base_sec: self.retry_after_base_sec,
                retry_after_jitter_sec: self.retry_after_jitter_sec,
            },
            brake_counters.clone(),
            // Dependency-free per-process jitter source (xorshift64*); only
            // consulted when retry_after_jitter_sec > 0.
            entropy_roll(),
        );

        // Real, non-recording transport: a plain tokio UDP socket. Bind into an
        // `Arc` so the endpoint can be SHARED: the core takes ownership of one
        // boxed clone (it drives the recv loop), while a second clone backs the
        // `UdpTransportMetrics` live `queueDepth`/`dropsTailDrop` getters.
        let net = RealSignalingNetwork::new();
        let endpoint: Arc<dyn UdpEndpoint> = net
            .bind_udp(BindUdpOpts::new(listen_sa, self.queue_max).with_pre_ingress(brake_hook))
            .await
            .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"))
            .into();
        let local = endpoint.local_addr();

        // The `UdpTransport` facade's Prometheus-visible shape: the brake
        // counters + live queue depth / queue_max / tail-drop proxied off the
        // bound endpoint. The buffered-send facets are permanently zero
        // (`BufferedUdpEndpoint` was a Node-era guard with no tokio analogue).
        let udp_metrics = {
            let ep_depth = endpoint.clone();
            let ep_tail = endpoint.clone();
            UdpTransportMetrics::new(
                self.queue_max,
                brake_counters.clone(),
                Arc::new(move || ep_depth.queue_depth() as u64),
                Arc::new(move || ep_tail.counters().tail_dropped),
            )
        };
        eprintln!(
            "{name} Tier-1 brake armed: stateless-503 new non-emergency INVITEs at \
             ingress depth >= {} (queue_max={}, tier1_pct={})",
            Tier1BrakeConfig {
                queue_max: self.queue_max,
                tier1_threshold_pct: self.udp_tier1_pct,
                retry_after_base_sec: self.retry_after_base_sec,
                retry_after_jitter_sec: self.retry_after_jitter_sec,
            }
            .threshold(),
            self.queue_max,
            self.udp_tier1_pct,
        );

        // Advertised SIP host:port stamped on every outbound Via / Contact /
        // b-leg Call-ID (see `b2bua::stack_identity`). It MUST be an address the
        // callee / proxy can route a response back to — the *bind* address may
        // be `0.0.0.0`, which is NOT routable: a peer's 200 OK to a Via/Contact
        // of `0.0.0.0` goes nowhere, the B2BUA never sees the answer, and it
        // retransmits the INVITE then CANCELs → a retransmission storm. Take
        // `B2BUA_ADVERTISE` (`host[:port]`) verbatim when set (k8s injects the
        // pod IP via the downward API `status.podIP`); otherwise fall back to
        // the bound address, coercing an unspecified `0.0.0.0` to loopback so
        // the literal is at least routable.
        let (advertise_ip, advertise_port) = match &self.advertise {
            Some(s) => {
                let s = s.trim();
                match s.rsplit_once(':').and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p))) {
                    Some((h, p)) => (h.to_string(), p),
                    // No `:port` (or unparseable port) → host-only; listen port.
                    None => (s.to_string(), local.port()),
                }
            }
            None => {
                let ip = if local.ip().is_unspecified() {
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                } else {
                    local.ip()
                };
                (ip.to_string(), local.port())
            }
        };
        eprintln!("{name} advertised SIP identity = {advertise_ip}:{advertise_port} (bind {local})");

        match &self.outbound_proxy {
            // Every b-leg request goes to the front proxy with a preloaded
            // `Route: <sip:host:port;lr;outbound>` so the proxy classifies it
            // worker-outbound and record-routes itself into the b-leg.
            Some((h, p)) => eprintln!(
                "{name} b-leg egress forced through front proxy {h}:{p} (all worker→callee SIP traverses the LB)"
            ),
            None => eprintln!(
                "{name} B2BUA_OUTBOUND_PROXY unset — b-leg goes pod-direct (local/dev only; NOT for the cluster)"
            ),
        }

        let config = B2buaConfig {
            self_ordinal: self.ordinal.clone(),
            sip_local_ip: advertise_ip,
            sip_local_port: advertise_port,
            b2b_outbound_proxy: self.outbound_proxy.clone(),
            cdr_buffer_queue_max: self.cdr_queue,
            event_dispatch_concurrency: self.concurrency,
            per_call_queue_cap: self.call_cap,
            max_messages_per_call: self.max_messages_per_call,
            keepalive_interval_sec: self.keepalive_sec,
            keepalive_timeout_sec: self.keepalive_timeout_sec,
            reboot_budget_sec: self.reboot_budget_sec,
            limiter_refresh_sec: self.limiter_refresh_sec,
            setup_timeout_sec: self.setup_timeout_sec,
            call_control_timeout_ms: self.call_control_timeout_ms,
            ack_timeout_sec: self.ack_timeout_sec,
            cps_bucket_size: self.cps_bucket_size,
            cps_bucket_rate: self.cps_bucket_rate,
            overload_panic_elu_threshold: self.overload_panic_elu_threshold,
            retry_after_base_sec: self.retry_after_base_sec,
            worker_allowed_target_suffixes: self.worker_allowed_target_suffixes.clone(),
            relay_headers: self.relay_headers.clone(),
            ..Default::default()
        };
        // Forbid booting with a config that would silently break HA: too-short a
        // keepalive, or a reboot budget that cannot outlast a primary reboot / a
        // keepalive refresh gap (which would self-evict healthy backups).
        config.validate().unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));

        RunnerBase {
            name: name.to_string(),
            endpoint,
            local,
            config,
            udp_metrics,
            // The shared registry, built BEFORE any deps so components a runner
            // constructs pre-spawn (notably CDR writers) record into the SAME
            // registry the core exports at `/metrics`.
            metrics: B2buaMetrics::new(),
            clock: Clock::system(),
            metrics_sa,
            env: self,
        }
    }
}

/// Comma-separated env list: trimmed, empties dropped.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Everything a bound runner shares between its assembly steps: the endpoint,
/// the validated config, and the process-wide metrics/clock. All fields pub so
/// a runner can reach any piece (e.g. `base.metrics` for a custom CDR sink,
/// `base.clock` for a replication store).
pub struct RunnerBase {
    /// Log-line prefix (the binary's name).
    pub name: String,
    /// The parsed env this base was built from (post any field overrides).
    pub env: RunnerEnv,
    /// The shared bound UDP endpoint (core recv loop + live metrics getters).
    pub endpoint: Arc<dyn UdpEndpoint>,
    /// The actually-bound local address.
    pub local: SocketAddr,
    /// The validated worker config (advertise coercion applied).
    pub config: B2buaConfig,
    /// The `UdpTransport` metrics facade over [`Self::endpoint`].
    pub udp_metrics: UdpTransportMetrics,
    /// The process-wide metrics registry the core exports at `/metrics`.
    pub metrics: B2buaMetrics,
    /// System wall clock (transaction/dialog timers fire for real).
    pub clock: Clock,
    /// Resolved probe/metrics HTTP listen address.
    pub metrics_sa: SocketAddr,
}

impl RunnerBase {
    /// The default call-limiter client: `HttpCallLimiter` against `LIMITER_URL`,
    /// or `NoopLimiter` (fail-open) when unset. A URL whose host cannot be
    /// resolved at boot also falls back to `NoopLimiter` (the worker still
    /// serves calls, unlimited, until restart) rather than crash-looping.
    pub fn limiter_from_env(&self) -> Arc<dyn CallLimiter> {
        if self.env.limiter_url.is_empty() {
            return Arc::new(NoopLimiter);
        }
        let hostport = self
            .env
            .limiter_url
            .strip_prefix("http://")
            .unwrap_or(&self.env.limiter_url)
            .trim_end_matches('/');
        match hostport.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => {
                eprintln!(
                    "call-limiter client -> {addr} (timeout {}ms, refresh {}s)",
                    self.env.limiter_timeout_ms, self.env.limiter_refresh_sec
                );
                Arc::new(HttpCallLimiter::new(
                    Arc::new(RealHttpNetwork::new()),
                    addr,
                    std::time::Duration::from_millis(self.env.limiter_timeout_ms),
                ))
            }
            None => {
                eprintln!(
                    "WARNING: LIMITER_URL {:?} did not resolve; running unlimited (NoopLimiter)",
                    self.env.limiter_url
                );
                Arc::new(NoopLimiter)
            }
        }
    }

    /// Production-shaped [`B2buaDeps`] defaults around the injected decision
    /// engine: in-memory store, limiter-from-env, `cdr_sink` (default
    /// [`NullCdrWriter`]) behind the bounded `BufferedCdrWriter`
    /// (drop-on-overload at `cdr_queue` depth), the shared metrics registry,
    /// entropy id-gen, no replication. Every field on the returned struct is
    /// pub — swap any single piece before [`spawn`](Self::spawn).
    pub fn deps(
        &self,
        decision: Arc<dyn CallDecisionEngine>,
        cdr_sink: Option<Arc<dyn CdrWriter>>,
    ) -> B2buaDeps {
        let sink = cdr_sink.unwrap_or_else(|| Arc::new(NullCdrWriter));
        B2buaDeps {
            config: self.config.clone(),
            decision,
            limiter: self.limiter_from_env(),
            cdr: Arc::new(BufferedCdrWriter::spawn(sink, self.env.cdr_queue, self.metrics.clone())),
            store: Arc::new(InMemoryCallStore::new()),
            clock: self.clock.clone(),
            id_gen: Arc::new(IdGen::from_entropy()),
            replication: None,
            metrics: self.metrics.clone(),
            // The generic service-authorable async-HTTP port is opt-in; a runner
            // that wants it swaps this field before `spawn` (every field is pub).
            adaptation_http: None,
        }
    }

    /// Spawn the core over one boxed clone of the shared endpoint, composing
    /// `services` (ADR-0016) above the defaults.
    pub fn spawn(&self, deps: B2buaDeps, services: Vec<ServiceDef>) -> Arc<B2buaCore> {
        Arc::new(B2buaCore::spawn_with_services(Box::new(self.endpoint.clone()), deps, services))
    }

    /// Readiness/metrics probe server (shared `probe-http`). The `/ready` state
    /// is a single read of one source: `core.readiness_state()` already folds
    /// in the Draining latch, so there is no second flag to drift from it. The
    /// `/metrics` body concatenates the worker's metric sources per scrape
    /// (core registry + txn backpressure + UDP transport + overload signal),
    /// then `extra_metrics` (e.g. allocator stats). Hold the returned server
    /// for the process lifetime — its accept loop aborts on drop.
    pub async fn spawn_probe_server(
        &self,
        core: &Arc<B2buaCore>,
        extra_metrics: Option<probe_http::MetricsFn>,
        heap: Option<probe_http::HeapDumpFn>,
    ) -> Option<probe_http::ProbeServer> {
        let core_ready = core.clone();
        let txn_metrics = core.txn_metrics().clone();
        // The worker-side overload signal (Tier-3 admission gate INPUTs +
        // DECISIONs + emergency-admit counter).
        let overload = core.overload().clone();
        let udp_metrics = self.udp_metrics.clone();
        let metrics = self.metrics.clone();
        let routes = probe_http::ProbeRoutes {
            metrics: Arc::new(move || {
                let mut text = metrics.prometheus_text();
                text.push_str(&txn_metrics_text(&txn_metrics));
                text.push_str(&udp_metrics.prometheus_text());
                text.push_str(&overload.prometheus_text());
                if let Some(extra) = &extra_metrics {
                    text.push_str(&extra());
                }
                text
            }),
            ready: Arc::new(move || match core_ready.readiness_state() {
                b2bua::repl::ReadinessState::Ready => probe_http::ProbeState::Ready,
                b2bua::repl::ReadinessState::Draining => probe_http::ProbeState::Draining,
                b2bua::repl::ReadinessState::NotReady => probe_http::ProbeState::NotReady,
            }),
            heap,
        };
        match probe_http::ProbeServer::start(self.metrics_sa, routes).await {
            Ok(server) => {
                eprintln!(
                    "{} metrics on http://{}/metrics (readiness /ready)",
                    self.name,
                    server.addr()
                );
                Some(server)
            }
            Err(e) => {
                eprintln!("{} metrics server failed to bind {}: {e}", self.name, self.metrics_sa);
                None
            }
        }
    }

    /// Memory-attribution sampler: push the store + replication map sizes into
    /// their gauges every 5 s so an RSS climb can be pinned to a specific map
    /// even when `active_calls` is flat, and physically reap expired backup
    /// bodies + changelog tombstones. `reap` is correct but must be actively
    /// DRIVEN in production — logical/lazy cleanup is bounded only by OOM (same
    /// lesson as the timer wheel). 5 s is well inside the scrape cadence; the
    /// sample is a couple of brief locks, off the call path.
    pub fn spawn_gauge_sampler(&self, core: &Arc<B2buaCore>) {
        let core = core.clone();
        let clock = self.clock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                core.sample_gauges();
                if let Some(repl) = core.repl_store() {
                    // Sample inner-store map sizes BEFORE reap so a leak is
                    // visible even if reap is the thing fixing it.
                    let (bodies, idx, _meta, tomb) = repl.map_lens();
                    core.metrics().set_store_map_sizes(bodies as u64, idx as u64, tomb as u64);
                    repl.reap(clock.now_ms()).await;
                }
            }
        });
    }

    /// Graceful shutdown: SIGTERM (k8s pod termination) latches Draining —
    /// OPTIONS self-reports 503 and the readiness probe flips NotReady so the
    /// proxy steers new calls away — then waits up to the drain grace
    /// (`B2BUA_DRAIN_GRACE_MS`) for in-flight calls to finish. Ctrl-C
    /// (interactive) exits immediately. Returns when the process should exit.
    pub async fn run_until_shutdown(&self, core: &Arc<B2buaCore>) {
        let name = &self.name;
        let drain_grace_ms = self.env.drain_grace_ms;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("{name} SIGINT — shutting down");
            }
            _ = wait_sigterm(name) => {
                eprintln!("{name} SIGTERM — begin draining ({drain_grace_ms}ms grace)");
                // Latch Draining, then wait for the live call map to clear —
                // capped at the grace. A node with no calls exits at once; a
                // busy node is bounded; a residual is logged, never silently cut.
                let residual = core.drain(std::time::Duration::from_millis(drain_grace_ms)).await;
                if residual == 0 {
                    eprintln!("{name} drained cleanly — exiting");
                } else {
                    eprintln!("{name} drain grace elapsed with {residual} call(s) still active — exiting");
                }
            }
        }
    }
}

/// Prometheus text for the sip-txn backpressure signals that `B2buaMetrics`
/// omits: events-channel depth/capacity, per-reason drop counters, and active
/// transactions. The `reason="response"` drop series is the keepalive-response
/// shedding that tears down established dialogs under a new-call burst —
/// invisible until this was exported.
pub fn txn_metrics_text(m: &sip_txn::TransactionMetrics) -> String {
    use sip_txn::EventQueueDropReason;
    let mut s = String::new();
    s.push_str("# HELP b2bua_txn_active_transactions In-flight client+server transactions.\n");
    s.push_str("# TYPE b2bua_txn_active_transactions gauge\n");
    s.push_str(&format!("b2bua_txn_active_transactions {}\n", m.active_transactions()));
    s.push_str("# HELP b2bua_txn_timer_queue_len Live entries in the txn-layer DelayQueue (retransmit/timeout/cleanup); a climb vs flat active_transactions is a timer/slab leak.\n");
    s.push_str("# TYPE b2bua_txn_timer_queue_len gauge\n");
    s.push_str(&format!("b2bua_txn_timer_queue_len {}\n", m.timer_queue_len()));
    s.push_str("# HELP b2bua_txn_retransmit_buf_bytes Sum of per-txn retransmit-buffer bytes retained for retransmission.\n");
    s.push_str("# TYPE b2bua_txn_retransmit_buf_bytes gauge\n");
    s.push_str(&format!("b2bua_txn_retransmit_buf_bytes {}\n", m.retransmit_buf_bytes()));
    s.push_str("# HELP b2bua_txn_event_queue_depth Inbound->app events channel current depth.\n");
    s.push_str("# TYPE b2bua_txn_event_queue_depth gauge\n");
    s.push_str(&format!("b2bua_txn_event_queue_depth {}\n", m.event_queue_depth()));
    s.push_str("# HELP b2bua_txn_event_queue_capacity Inbound->app events channel capacity.\n");
    s.push_str("# TYPE b2bua_txn_event_queue_capacity gauge\n");
    s.push_str(&format!("b2bua_txn_event_queue_capacity {}\n", m.event_queue_capacity()));
    s.push_str("# HELP b2bua_txn_event_queue_drops_total Events shed when the inbound->app channel was full, by class.\n");
    s.push_str("# TYPE b2bua_txn_event_queue_drops_total counter\n");
    for r in EventQueueDropReason::ALL {
        s.push_str(&format!(
            "b2bua_txn_event_queue_drops_total{{reason=\"{}\"}} {}\n",
            r.label(),
            m.event_queue_drops(r)
        ));
    }
    s
}

/// Await a SIGTERM (k8s sends this on pod termination). On non-unix this future
/// never resolves (only Ctrl-C drives shutdown there).
#[cfg(unix)]
async fn wait_sigterm(name: &str) {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            eprintln!("{name} cannot install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm(_name: &str) {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suffixes(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ip_literal_dest_is_admissible_regardless_of_suffixes() {
        // The default B2BUA_DEST (127.0.0.1) and any IP literal must boot even
        // with an empty allow-list.
        assert!(validate_default_dest("127.0.0.1", &suffixes(&[])).is_ok());
    }

    #[test]
    fn dest_matching_a_configured_suffix_boots() {
        assert!(validate_default_dest(
            "sipp-uas.sip-test.svc.cluster.local",
            &suffixes(&[".svc.cluster.local"])
        )
        .is_ok());
    }

    #[test]
    fn dest_excluded_by_its_own_allow_list_refuses_boot() {
        let e = validate_default_dest("bob", &suffixes(&[".svc.cluster.local"]))
            .expect_err("a default callee its own allow-list rejects must fail boot");
        assert!(e.contains("B2BUA_DEST"), "msg was: {e}");
    }

    #[test]
    fn star_wildcard_admits_any_dest() {
        assert!(validate_default_dest("bob", &suffixes(&["*"])).is_ok());
    }

    #[test]
    fn tier1_pct_out_of_range_refuses_boot() {
        assert!(validate_tier1_pct(0).is_err());
        assert!(validate_tier1_pct(101).is_err());
        // boundaries are in range
        assert!(validate_tier1_pct(1).is_ok());
        assert!(validate_tier1_pct(100).is_ok());
    }

    #[test]
    fn split_host_port_defaults_and_parses() {
        assert_eq!(split_host_port("bob:5071"), ("bob".to_string(), 5071));
        assert_eq!(split_host_port("bob"), ("bob".to_string(), 5060));
        assert_eq!(split_host_port("bob:nope"), ("bob".to_string(), 5060));
    }

    #[test]
    fn truthy_flag_grammar() {
        for t in ["1", "true", "YES", " on "] {
            assert!(is_truthy(t), "{t:?} must be truthy");
        }
        for f in ["0", "", "off", "nope"] {
            assert!(!is_truthy(f), "{f:?} must be falsy");
        }
    }
}
