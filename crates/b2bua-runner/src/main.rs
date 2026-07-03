//! Standalone, containerizable B2BUA worker process.
//!
//! Wires the `b2bua` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork` — no `Recorder` decorator, no simulated
//! fabric) and a **system wall clock** (`Clock::system`, so transaction/dialog
//! timers fire). Production-shaped deps:
//!   - CDR  : `BufferedCdrWriter` (drop-on-overload) over a discarding sink, so
//!            an endurance run does not accumulate records in memory.
//!   - store: `InMemoryCallStore` (the only ported store; HA/Redis deferred).
//!   - limit: `HttpCallLimiter` when `LIMITER_URL` is set, else `NoopLimiter`
//!            (fail-open). See the `LIMITER_*` env below.
//!   - route: `ScriptedDecisionEngine::route_all_to_with_limiter(DEST, stress)`
//!            (the HTTP call-control adapter is a deferred slice; routing all
//!            calls to a fixed UAS mirrors the k8s `worker -> sipp-uas` topology).
//!            It attaches an always-on `B2BUA_STRESS_LIMITER` entry to every call
//!            (full-chain stress) and honors an inbound `X-Api-Call` `call_limiter`
//!            array so a dedicated stream can enforce a real cap.
//! It also serves a Prometheus `/metrics` + `/healthz` endpoint so the endurance
//! recorder can scrape worker application metrics alongside container CPU/mem.
//!
//! Config via env (all optional):
//!   B2BUA_LISTEN    SIP/signaling listen addr        (default 0.0.0.0:5060)
//!   B2BUA_ADVERTISE SIP host[:port] stamped on Via/Contact/b-leg Call-ID
//!                   (default: bound IP, or loopback if bind is 0.0.0.0).
//!                   In k8s inject the pod IP via downward API `status.podIP`,
//!                   else peers route responses to 0.0.0.0 (a storm).
//!   B2BUA_DEST      downstream UAS host:port          (default 127.0.0.1:5070)
//!   B2BUA_OUTBOUND_PROXY  front-proxy host:port every b-leg (worker→callee)
//!                   request is forced through (preloaded `Route ;lr;outbound`).
//!                   REQUIRED in the k8s cluster: a peer's internal pod IP is not
//!                   routable peer-to-peer in a real deployment, so ALL outbound
//!                   SIP must traverse the LB proxy — never go pod-direct. Unset →
//!                   b-leg goes straight to the callee (local/dev only). (unset)
//!   B2BUA_METRICS   Prometheus HTTP listen addr       (default 0.0.0.0:9091)
//!   B2BUA_QUEUE     inbound UDP queue depth (packets)  (default 8192)
//!   B2BUA_ORDINAL   worker ordinal stamped in callRef  (default w0)
//!   B2BUA_CDR_QUEUE buffered-CDR submit queue depth    (default 1024)
//!   B2BUA_CONCURRENCY handler concurrency ceiling       (default 8192; safety, not a rate cap)
//!   B2BUA_CALL_CAP  max concurrent calls before drop    (default 1_000_000)
//!   B2BUA_KEEPALIVE_SEC in-dialog OPTIONS keepalive interval (default 300 = 5 min, min 120)
//!   B2BUA_REBOOT_BUDGET_SEC replicated-backup TTL / reboot budget (default 600; min 60 and >= keepalive)
//!   B2BUA_SETUP_TIMEOUT_SEC a-leg total setup deadline, reroutes included (default 150, < the 158 s txn backstop; <= 0 disables)
//!   B2BUA_CALL_CONTROL_TIMEOUT_MS decision-backend deadline per round-trip (default 5000; <= 0 disables — ADR-0022)
//!   WORKER_ALLOWED_TARGET_SUFFIXES b-leg target-admission allow-list, comma-separated (default .svc.cluster.local; `*` = allow all, rollback sentinel; non-IP non-matching hosts are 503'd pre-leg)
//!   B2BUA_RELAY_HEADERS opt-in transparent header relay, comma-separated names copied from the a-leg INVITE onto every originated b-leg INVITE (default empty = no relay; structural headers never relayable)
//!
//! ## Call limiter
//!   LIMITER_URL             shared limiter base URL; unset → NoopLimiter (fail-open)
//!   LIMITER_WINDOW_SECONDS  refresh cadence; MUST match the service window (default 300)
//!   LIMITER_TIMEOUT_MS      per-request fail-open budget                     (default 150)
//!   B2BUA_STRESS_LIMITER_ID always-on limiter id on every call; "" disables  (default global-stress)
//!   B2BUA_STRESS_LIMITER_LIMIT cap for that entry (never rejects in practice) (default 999999)
//!
//! ## HA replication (S11) — opt-in via `B2BUA_REPL=1` (default off = legacy)
//!   B2BUA_REPL          "1"/"true" enables peer-to-peer call replication
//!   B2BUA_REPL_LISTEN   replication TCP listen addr     (default 0.0.0.0:9092)
//!   B2BUA_REPL_PORT     port peers are reached on       (default = REPL_LISTEN port)
//!   B2BUA_PEERS         static membership `ord@host,..`  (dev/local; takes precedence)
//!   B2BUA_REPL_SERVICE  headless Service to discover     (default b2bua-worker)
//!   B2BUA_NAMESPACE     namespace for k8s discovery      (default $POD_NAMESPACE / sip-test)
//!
//! Two deferred S11 decisions are resolved here:
//!   - **Incarnation gen** = boot wall-clock seconds (monotonic across pod
//!     restarts → `(new_gen,0) > (old_gen,*)` holds; see [`boot_incarnation`]).
//!   - **Replication addressing** = port-agnostic `Peer.host` + a cluster-wide
//!     `B2BUA_REPL_PORT` (see [`make_addr_resolver`]) — no per-peer port grammar.
//! And SIGTERM latches the worker into `Draining` (OPTIONS 503 + readiness
//! probe fails) so k8s steers new calls away while in-flight calls finish.

// Use jemalloc instead of the glibc system allocator. Under the many tokio
// worker threads, glibc malloc spawns up to 8×ncpu arenas and retains freed
// chunks (it caps arena *count*, not per-arena high-water mark), so a churning
// SIP B2BUA's RSS ratchets monotonically up under sustained load and never
// returns memory to the OS — a 2026-06-13/14 no-chaos soak measured ~209 MiB/h
// growth with all logical state (active_calls/store/txn/repl) dead flat, leading
// to a node-cgroup OOM. jemalloc's decay-based purging returns dirty/muzzy pages
// to the OS (tuned aggressively via _RJEM_MALLOC_CONF on the worker container),
// bounding steady-state RSS. No logical leak exists; this is purely allocator
// retention. See deploy/k8s/manifests/20-worker.yaml.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::{CallLimiterEntry, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::{B2buaMetrics, UdpTransportMetrics};
use b2bua::repl::{PeerResolver, ReplicatingCallStore};
use b2bua::store::InMemoryCallStore;
use b2bua::target_admission::{classify_admission, AdmissionVerdict};
use b2bua::tier1_brake::{
    build_tier1_brake_hook, entropy_roll, Tier1BrakeConfig, Tier1BrakeCounters,
};
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use call::Call;
use http_net::RealHttpNetwork;
use repl_net::RealReplicationNetwork;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_txn::IdGen;
use topology::{Membership, Peer, StaticMembership};

mod cdr_rabbitmq;

/// A CDR sink that discards every record. Production B2BUA writes CDRs to an
/// external store; that adapter is a deferred slice, and for load/endurance we
/// must not accumulate records in process memory. Wrapped by `BufferedCdrWriter`
/// so the buffer/drainer machinery is still exercised.
struct NullCdrWriter;

#[async_trait]
impl CdrWriter for NullCdrWriter {
    async fn write(&self, _call: &Call, _terminated_at: i64) {}
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Truthy env flag: `1`/`true`/`yes`/`on` (case-insensitive) → true.
fn env_flag(key: &str) -> bool {
    matches!(
        env_or(key, "0").trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Always-on "stress" limiter entry attached to every routed call so the full
/// admit/release/refresh chain is exercised on all traffic (the endurance suite
/// drives this). `B2BUA_STRESS_LIMITER_ID` empty disables it; the default cap
/// (`B2BUA_STRESS_LIMITER_LIMIT`, default 999999) is high enough to never reject.
fn stress_limiter_from_env() -> Option<CallLimiterEntry> {
    let id = env_or("B2BUA_STRESS_LIMITER_ID", "global-stress");
    if id.trim().is_empty() {
        return None;
    }
    let limit = env::var("B2BUA_STRESS_LIMITER_LIMIT")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(999_999);
    Some(CallLimiterEntry { id, limit })
}

fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| panic!("cannot resolve {addr:?}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("no address resolved from {addr:?}"))
}

/// Boot-time coherence checks on runner-only inputs that [`B2buaConfig::validate`]
/// can't see: the worker's default callee host vs. its OWN TargetAdmission
/// allow-list, and the Tier-1 brake percentage. Returns the first violation; the
/// runner refuses to boot on `Err`. Pure (no env / IO) so it is unit-testable.
fn validate_runner_overrides(
    dest_host: &str,
    worker_allowed_target_suffixes: &[String],
    udp_tier1_pct: u32,
) -> Result<(), String> {
    if !(1..=100).contains(&udp_tier1_pct) {
        return Err(format!(
            "B2BUA_UDP_TIER1_PCT={udp_tier1_pct} out of range 1..=100: the Tier-1 \
             brake fires at ingress depth >= queue_max × pct/100, so 0 would shed \
             every packet and >100 would never fire. Use 1..=100 (100 = brake only \
             when the queue is full)"
        ));
    }
    // The b-leg admission gate (apply_route) classifies the callee's
    // `destination.host`; the static default (`B2BUA_DEST`) must itself be
    // admissible, else EVERY default-routed call is 503'd before the b-leg and the
    // worker can never serve one. Per-call `X-Api-Call` destinations are runtime
    // and not checkable here, but a default that the worker's own allow-list
    // rejects is an unambiguous misconfiguration we refuse to boot on.
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

/// Split a `host:port` into its parts WITHOUT DNS resolution (the host may be a
/// service name resolved per-call downstream). Port defaults to 5060 if absent or
/// unparseable. Used for the b-leg callee (`B2BUA_DEST`) — see the call site for
/// why it must stay unresolved.
fn split_host_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5060)),
        None => (s.to_string(), 5060),
    }
}

/// **Incarnation gen** (deferred S11 decision: boot wall-clock vs pod epoch).
/// We pick **boot wall-clock milliseconds**: normally monotonic across pod
/// restarts, so a rebooted worker serves under a higher `gen` than its previous
/// life — `(new_gen, 0) > (old_gen, *)` — and pullers apply its frames without
/// a manual reset (ADR-0011 X9). Milliseconds (not seconds) so a sub-second
/// crash-restart cannot reuse the previous life's gen with the counter reset to
/// 0 — under the old seconds gen a warm peer kept tailing from its stale high
/// counter and silently skipped every new entry. The wall clock can still step
/// BACKWARD (NTP/VM resync); that case — and any residual collision — is
/// handled server-side: `Changelog::needs_reset` forces a `ResetToBootstrap`
/// whenever a puller presents a same-gen counter above our head or a
/// future-gen watermark. Falls back to 0 only if the wall clock is before the
/// epoch (never, in practice).
fn boot_incarnation() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where replication peer addresses come from (ADR-0012 D3).
enum ReplAddressing {
    /// Static `B2BUA_PEERS`: the host is a bare IP or a user-provided DNS name —
    /// use it directly (IP fast-path, else resolve the name).
    Static,
    /// k8s informer: derive the **stable per-pod FQDN** from the ordinal (=
    /// StatefulSet pod name) and resolve it FRESH per connect, so a restarted
    /// peer's new IP is picked up without a membership delta. Falls back to the
    /// EndpointSlice-supplied host (a Pod IP) if CoreDNS misses, so we are not
    /// hard-dependent on DNS for liveness.
    K8sPodDns { service: String, namespace: String },
}

/// **Replication addressing** (ADR-0012 D3 / deferred S11 decision). Membership is
/// port-agnostic (a Pod has one IP, many ports); every peer's repl server is at
/// `<resolved-host>:repl_port`, where `repl_port` is one cluster-wide config value
/// (`B2BUA_REPL_PORT`). The address is resolved **fresh on every connect attempt**
/// so a restarted peer self-heals via the puller's own reconnect loop. `None`
/// (unresolvable right now) → the puller backs off and retries, re-resolving.
struct ReplResolver {
    repl_port: u16,
    addressing: ReplAddressing,
}

#[async_trait]
impl PeerResolver for ReplResolver {
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr> {
        let addr = match &self.addressing {
            ReplAddressing::Static => {
                if let Ok(ip) = peer.host.parse::<IpAddr>() {
                    Some(SocketAddr::new(ip, self.repl_port))
                } else {
                    tokio::net::lookup_host((peer.host.as_str(), self.repl_port))
                        .await
                        .ok()
                        .and_then(|mut it| it.next())
                }
            }
            ReplAddressing::K8sPodDns { service, namespace } => {
                // Prefer the stable per-pod DNS name (D3): re-resolving it picks up
                // a restarted peer's new IP without any membership delta.
                let fqdn =
                    format!("{}.{}.{}.svc.cluster.local", peer.ordinal, service, namespace);
                let by_dns = tokio::net::lookup_host((fqdn.as_str(), self.repl_port))
                    .await
                    .ok()
                    .and_then(|mut it| it.next());
                // CoreDNS miss / NXDOMAIN-while-not-ready → fall back to the
                // EndpointSlice host (a Pod IP). Backoff+retry covers transients.
                by_dns.or_else(|| {
                    peer.host
                        .parse::<IpAddr>()
                        .ok()
                        .map(|ip| SocketAddr::new(ip, self.repl_port))
                })
            }
        };
        // Fires once per (re)connect attempt — its presence (with a *new* addr)
        // proves the puller redirected to a restarted peer (handoff §7 / ADR-0012
        // D3). `None` → unresolvable now; the puller backs off and retries.
        match addr {
            Some(a) => eprintln!("b2bua-runner repl: peer={} resolved -> {a}", peer.ordinal),
            None => eprintln!("b2bua-runner repl: peer={} unresolvable (will retry)", peer.ordinal),
        }
        addr
    }
}

fn make_addr_resolver(repl_port: u16, addressing: ReplAddressing) -> b2bua::repl::AddrResolver {
    Arc::new(ReplResolver { repl_port, addressing })
}

/// Resolve cluster membership for replication. `B2BUA_PEERS` (a static
/// `ord@host,..` list, used for dev/local) takes precedence; otherwise the k8s
/// EndpointSlice informer watches the headless `B2BUA_REPL_SERVICE`. Returns
/// `None` (→ replication stays off) if neither a static list nor an in-cluster
/// kube client is available — liveness over completeness, the worker still
/// serves SIP.
async fn build_membership() -> Option<(Arc<dyn Membership>, ReplAddressing)> {
    let peers = env_or("B2BUA_PEERS", "");
    if !peers.trim().is_empty() {
        match StaticMembership::from_string(&peers, "B2BUA_PEERS") {
            Ok(m) => {
                eprintln!("b2bua-runner replication membership: static B2BUA_PEERS={peers}");
                return Some((Arc::new(m), ReplAddressing::Static));
            }
            Err(e) => {
                eprintln!("b2bua-runner B2BUA_PEERS parse error: {e} — replication disabled");
                return None;
            }
        }
    }
    let service = env_or("B2BUA_REPL_SERVICE", "b2bua-worker");
    let namespace =
        env::var("B2BUA_NAMESPACE").or_else(|_| env::var("POD_NAMESPACE")).unwrap_or_else(|_| "sip-test".to_string());
    // rustls 0.23 has no default CryptoProvider compiled in; install ring once
    // before the kube client opens its first TLS connection (idempotent — a
    // second call returns Err, which we ignore).
    let _ = rustls::crypto::ring::default_provider().install_default();
    match kube::Client::try_default().await {
        Ok(client) => {
            eprintln!(
                "b2bua-runner replication membership: k8s EndpointSlice informer (svc={service}, ns={namespace})"
            );
            // Reach peers by their stable per-pod DNS name (ADR-0012 D3), built from
            // the ordinal + this Service + namespace.
            let addressing = ReplAddressing::K8sPodDns { service: service.clone(), namespace: namespace.clone() };
            Some((Arc::new(topology::K8sMembership::spawn(client, namespace, service)), addressing))
        }
        Err(e) => {
            eprintln!("b2bua-runner no kube client ({e}) and no B2BUA_PEERS — replication disabled");
            None
        }
    }
}

/// Prometheus text for the sip-txn backpressure signals that `B2buaMetrics`
/// omits: events-channel depth/capacity, per-reason drop counters, and active
/// transactions. The `reason="response"` drop series is the keepalive-response
/// shedding that tears down established dialogs under a new-call burst — invisible
/// until this was exported.
fn txn_metrics_text(m: &sip_txn::TransactionMetrics) -> String {
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

#[tokio::main]
async fn main() {
    // Loud confirmation the jemalloc decay config (_RJEM_MALLOC_CONF) actually
    // parsed — a typo is silently ignored. Mirrored by the jemalloc_opt_*_decay_ms
    // gauges on /metrics.
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();

    let listen = env_or("B2BUA_LISTEN", "0.0.0.0:5060");
    let dest = env_or("B2BUA_DEST", "127.0.0.1:5070");
    let metrics_addr = env_or("B2BUA_METRICS", "0.0.0.0:9091");
    let queue_max: usize = env_or("B2BUA_QUEUE", "8192").parse().expect("B2BUA_QUEUE");
    let cdr_queue: usize = env_or("B2BUA_CDR_QUEUE", "1024").parse().expect("B2BUA_CDR_QUEUE");
    let ordinal = env_or("B2BUA_ORDINAL", "w0");
    // Dispatch throttle ceilings — handler concurrency + max concurrent calls.
    // Set deliberately high so they never cap throughput below the offered rate
    // (they are back-pressure SAFETY limits, not a rate governor); raise via env
    // if a load test ever approaches them. `cap_drops`/`saturation` metrics flag
    // if either is actually hit.
    let concurrency: usize = env_or("B2BUA_CONCURRENCY", "8192").parse().expect("B2BUA_CONCURRENCY");
    let call_cap: usize = env_or("B2BUA_CALL_CAP", "1000000").parse().expect("B2BUA_CALL_CAP");
    // MAX_MESSAGES_PER_CALL cap-defense (loop/runaway guard). TS default 100;
    // we default 200 (headroom for a multi-hour keepalive-held call). Enforced
    // in the router's in-dialog path — a call past the cap is begin-terminated.
    let max_messages_per_call: u64 =
        env_or("B2BUA_MAX_MESSAGES_PER_CALL", "200").parse().expect("B2BUA_MAX_MESSAGES_PER_CALL");
    // In-dialog OPTIONS keepalive interval (seconds). Production default 300 s
    // (5 min); a shorter poke breaks long-hold endurance traffic.
    let keepalive_sec: i64 = env_or("B2BUA_KEEPALIVE_SEC", "300").parse().expect("B2BUA_KEEPALIVE_SEC");
    // In-dialog OPTIONS keepalive-timeout grace (seconds): wait for the OPTIONS 200
    // before declaring the leg dead and BYE-ing. Default 32 s (was a hard 5 s) so a
    // reclaimed dialog's keepalive can round-trip across the post-reboot recovery
    // window (smoothed reclaim burst + proxy re-discovering the new pod IP).
    let keepalive_timeout_sec: i64 =
        env_or("B2BUA_KEEPALIVE_TIMEOUT_SEC", "32").parse().expect("B2BUA_KEEPALIVE_TIMEOUT_SEC");
    // Replicated-backup TTL ("reboot budget"): how long a backup Element survives
    // without a refresh from its primary. Decoupled from the keepalive but must
    // outlast it — enforced by `config.validate()` below.
    let reboot_budget_sec: i64 =
        env_or("B2BUA_REBOOT_BUDGET_SEC", "600").parse().expect("B2BUA_REBOOT_BUDGET_SEC");
    // Call-level a-leg setup deadline (seconds): caller's total wait for a final
    // response, reroutes included. Ledger-replicated (survives crash → reclaim,
    // unlike the sip-txn 158 s backstop). Keep below 158; <= 0 disables.
    let setup_timeout_sec: i64 =
        env_or("B2BUA_SETUP_TIMEOUT_SEC", "150").parse().expect("B2BUA_SETUP_TIMEOUT_SEC");
    // Decision-backend deadline (ms) per round-trip; the core wraps the injected
    // engine so a hung/3rd-party adapter can never strand a caller past this
    // bound (ADR-0022). <= 0 disables.
    let call_control_timeout_ms: i64 = env_or("B2BUA_CALL_CONTROL_TIMEOUT_MS", "5000")
        .parse()
        .expect("B2BUA_CALL_CONTROL_TIMEOUT_MS");

    // ACK-timeout grace (seconds, RFC 3261 §13.3.1.4): the 2xx-without-ACK
    // give-up window (RFC 64·T1 = 32 s). Below this the un-ACKed answered call is
    // retransmitted then BYE'd both legs; <= 0 disables. See `B2buaConfig`.
    let ack_timeout_sec: i64 =
        env_or("B2BUA_ACK_TIMEOUT_SEC", "32").parse().expect("B2BUA_ACK_TIMEOUT_SEC");

    // Tier-3 admission gate (migration/09 — port of CPS_BUCKET_SIZE /
    // CPS_BUCKET_RATE / OVERLOAD_PANIC_ELU_THRESHOLD / RETRY_AFTER_BASE_SEC). The
    // hard CPS ceiling + panic-ELU backstop the worker enforces on new INVITEs.
    let cps_bucket_size: u32 =
        env_or("B2BUA_CPS_BUCKET_SIZE", "1000").parse().expect("B2BUA_CPS_BUCKET_SIZE");
    let cps_bucket_rate: u32 =
        env_or("B2BUA_CPS_BUCKET_RATE", "500").parse().expect("B2BUA_CPS_BUCKET_RATE");
    let overload_panic_elu_threshold: f64 = env_or("B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD", "0.75")
        .parse()
        .expect("B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD");
    let retry_after_base_sec: u32 =
        env_or("B2BUA_RETRY_AFTER_BASE_SEC", "5").parse().expect("B2BUA_RETRY_AFTER_BASE_SEC");
    // b-leg target-admission allow-list (port of the `AppConfig` env read for
    // `workerAllowedTargetSuffixes`): comma-separated, trimmed, empties dropped.
    // Default is the single K8s in-cluster DNS suffix `.svc.cluster.local`, so
    // production pod FQDNs pass and a bogus host is 503'd pre-leg; `*` is the
    // rollback sentinel (matches every host).
    let worker_allowed_target_suffixes: Vec<String> =
        env_or("WORKER_ALLOWED_TARGET_SUFFIXES", ".svc.cluster.local")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

    // Tier-1 overload brake (this item — port of the `UdpTransport` `preIngress`
    // policy). At inbound-queue depth >= floor(B2BUA_QUEUE * pct / 100) a new,
    // non-emergency INVITE is shed with a STATELESS 503 before the parser runs —
    // the cheapest shed in the stack, ahead of the Tier-3 admission gate. TS
    // `udpQueueTier1ThresholdPct` env default 70; `retryAfterJitterSec` default 5
    // (the 503's Retry-After is `retry_after_base_sec` + U[0,jitter]). `pct = 0`
    // sets the threshold to 0 (brake from the first INVITE — a deliberate
    // emergency lever, not a default); `pct >= 100` effectively disables it (the
    // queue tail-drops before depth can reach queue_max).
    let udp_tier1_pct: u32 =
        env_or("B2BUA_UDP_TIER1_PCT", "70").parse().expect("B2BUA_UDP_TIER1_PCT");
    let retry_after_jitter_sec: u32 = env_or("B2BUA_RETRY_AFTER_JITTER_SEC", "5")
        .parse()
        .expect("B2BUA_RETRY_AFTER_JITTER_SEC");

    // Call limiter. Unset LIMITER_URL → NoopLimiter (today's non-limiting
    // behaviour). The refresh cadence MUST match the limiter's window seconds.
    let limiter_url = env_or("LIMITER_URL", "");
    let limiter_timeout_ms: u64 = env_or("LIMITER_TIMEOUT_MS", "150").parse().unwrap_or(150);
    let limiter_refresh_sec: i64 = env_or("LIMITER_WINDOW_SECONDS", "300").parse().unwrap_or(300);

    let listen_sa = resolve(&listen);
    // The b-leg callee (`B2BUA_DEST`) is passed to the decision engine as an
    // UNRESOLVED host:port. A DNS name is resolved PER CALL — and round-robined
    // across a headless Service's pod set — in b2bua's `apply_route`, so the b-leg
    // goes pod-direct from the LB VIP with no kube-proxy ClusterIP NAT. Resolving
    // once here would instead pin every call to a single startup-resolved pod (and
    // could fail the worker's boot if the callee Service has no endpoints yet). An
    // IP literal passes straight through the resolver unchanged.
    let (dest_host, dest_port) = split_host_port(&dest);
    let metrics_sa = resolve(&metrics_addr);

    // Tier-1 overload brake: build the `preIngress` hook + its counters and
    // install them on the worker socket. THIS is the production wiring the
    // migration item is about — without it the brake (the cheapest stateless-503
    // shed, ahead of Tier-3) is absent and a flooded ingress queue tail-drops new
    // INVITEs silently instead of returning a routable 503 + Retry-After. The
    // counters are retained for the `/metrics` scrape (port of the
    // `UdpTransportMetrics.dropsTier1Brake` / `tier1RejectSent` surface).
    let brake_counters = Tier1BrakeCounters::new();
    let brake_hook = build_tier1_brake_hook(
        Tier1BrakeConfig {
            queue_max,
            tier1_threshold_pct: udp_tier1_pct,
            retry_after_base_sec,
            retry_after_jitter_sec,
        },
        brake_counters.clone(),
        // Dependency-free per-process jitter source (xorshift64*); only consulted
        // when retry_after_jitter_sec > 0.
        entropy_roll(),
    );

    // Real, non-recording transport: a plain tokio UDP socket. Bind into an
    // `Arc` so the endpoint can be SHARED: the core takes ownership of one boxed
    // clone (it drives the recv loop), while a second clone backs the
    // `UdpTransportMetrics` live `queueDepth`/`dropsTailDrop` getters below — the
    // same endpoint the transport sends through, exactly as the TS `UdpTransport`
    // facade closes over `endpoint` for both `send` and `registry.udp`.
    let net = RealSignalingNetwork::new();
    let endpoint: Arc<dyn sip_net::UdpEndpoint> = net
        .bind_udp(BindUdpOpts::new(listen_sa, queue_max).with_pre_ingress(brake_hook))
        .await
        .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"))
        .into();
    let local = endpoint.local_addr();

    // The `UdpTransport` facade's Prometheus-visible shape (port of
    // `UdpTransportMetrics`): the brake counters + live queue depth / queue_max /
    // tail-drop proxied off the bound endpoint. The buffered-send facets are
    // permanently zero: `BufferedUdpEndpoint` was removed (won't port) — it was a
    // Node-era guard against blocking `getaddrinfo` in `send`, which has no
    // analogue in tokio (sends take an already-resolved `SocketAddr`), and the
    // b2bua sends straight through this endpoint.
    // This SUPERSEDES the standalone `tier1_brake_metrics_text` on `/metrics`.
    let udp_metrics = {
        let ep_depth = endpoint.clone();
        let ep_tail = endpoint.clone();
        UdpTransportMetrics::new(
            queue_max,
            brake_counters.clone(),
            Arc::new(move || ep_depth.queue_depth() as u64),
            Arc::new(move || ep_tail.counters().tail_dropped),
        )
    };
    eprintln!(
        "b2bua-runner Tier-1 brake armed: stateless-503 new non-emergency INVITEs at \
         ingress depth >= {} (queue_max={queue_max}, tier1_pct={udp_tier1_pct})",
        Tier1BrakeConfig {
            queue_max,
            tier1_threshold_pct: udp_tier1_pct,
            retry_after_base_sec,
            retry_after_jitter_sec,
        }
        .threshold()
    );

    // Advertised SIP host:port stamped on every outbound Via / Contact / b-leg
    // Call-ID (see `b2bua::stack_identity`). It MUST be an address the callee /
    // proxy can route a response back to — the *bind* address is `0.0.0.0` (bind
    // on all interfaces), which is NOT routable: a peer's 200 OK to a Via/Contact
    // of `0.0.0.0` goes nowhere, the B2BUA never sees the answer, and it
    // retransmits the INVITE then CANCELs → a retransmission storm that floods
    // the UAS. Mirror the proxy's `PROXY_ADVERTISE` pattern: take `B2BUA_ADVERTISE`
    // (`host[:port]`) verbatim when set (k8s injects the pod IP via the downward
    // API `status.podIP`); otherwise fall back to the bound address, coercing an
    // unspecified `0.0.0.0` to loopback so the literal is at least routable.
    let (advertise_ip, advertise_port) = match env::var("B2BUA_ADVERTISE") {
        Ok(s) => {
            let s = s.trim();
            match s.rsplit_once(':').and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p))) {
                Some((h, p)) => (h.to_string(), p),
                // No `:port` (or unparseable port) → host-only; use the listen port.
                None => (s.to_string(), local.port()),
            }
        }
        Err(_) => {
            let ip = if local.ip().is_unspecified() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                local.ip()
            };
            (ip.to_string(), local.port())
        }
    };
    eprintln!(
        "b2bua-runner advertised SIP identity = {advertise_ip}:{advertise_port} (bind {local})"
    );

    // Front-proxy egress. Every b-leg (worker→callee) request is sent to this
    // `host:port` with a preloaded `Route: <sip:host:port;lr;outbound>` so the
    // proxy classifies it worker-outbound, strips the Route, forwards to the
    // callee, and record-routes itself into the b-leg — keeping in-dialog
    // BYE/OPTIONS/re-INVITE on the proxy path too. REQUIRED in the cluster: a
    // peer's internal pod IP is NOT routable peer-to-peer, so all SIP MUST go
    // through the LB proxy, never pod-direct. Unset → b-leg goes straight to the
    // callee (local/dev only). Format `host:port`; a bad value is fatal (a
    // silent fallback to pod-direct is exactly the endurance bug this prevents).
    let b2b_outbound_proxy: Option<(String, u16)> = match env::var("B2BUA_OUTBOUND_PROXY") {
        Ok(s) if !s.trim().is_empty() => {
            let s = s.trim();
            let (h, p) = s
                .rsplit_once(':')
                .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h.to_string(), p)))
                .unwrap_or_else(|| panic!("B2BUA_OUTBOUND_PROXY must be host:port, got {s:?}"));
            eprintln!(
                "b2bua-runner b-leg egress forced through front proxy {h}:{p} (all worker→callee SIP traverses the LB)"
            );
            Some((h, p))
        }
        _ => {
            eprintln!(
                "b2bua-runner B2BUA_OUTBOUND_PROXY unset — b-leg goes pod-direct (local/dev only; NOT for the cluster)"
            );
            None
        }
    };

    // Opt-in transparent header relay: comma-separated header names copied
    // verbatim from the a-leg INVITE onto every originated b-leg INVITE (callee
    // + REFER transfer leg). Unset/empty = no relay (production default, no-op).
    let relay_headers: Vec<String> = env_or("B2BUA_RELAY_HEADERS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let config = B2buaConfig {
        self_ordinal: ordinal.clone(),
        sip_local_ip: advertise_ip,
        sip_local_port: advertise_port,
        b2b_outbound_proxy,
        cdr_buffer_queue_max: cdr_queue,
        event_dispatch_concurrency: concurrency,
        per_call_queue_cap: call_cap,
        max_messages_per_call,
        keepalive_interval_sec: keepalive_sec,
        keepalive_timeout_sec,
        reboot_budget_sec,
        limiter_refresh_sec,
        setup_timeout_sec,
        call_control_timeout_ms,
        ack_timeout_sec,
        cps_bucket_size,
        cps_bucket_rate,
        overload_panic_elu_threshold,
        retry_after_base_sec,
        worker_allowed_target_suffixes,
        relay_headers,
        ..Default::default()
    };
    // Forbid booting with a config that would silently break HA: too-short a
    // keepalive, or a reboot budget that cannot outlast a primary reboot / a
    // keepalive refresh gap (which would self-evict healthy backups).
    config
        .validate()
        .unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));
    // Runner-only coherence (not visible to `B2buaConfig::validate`): the default
    // callee must be admissible under the worker's own allow-list, and the Tier-1
    // brake percentage must be in range. Refuse to boot with a clear message
    // rather than silently 503 every call / shed every packet.
    validate_runner_overrides(
        &dest_host,
        &config.worker_allowed_target_suffixes,
        udp_tier1_pct,
    )
    .unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));

    // Build the limiter client now that config is settled. A LIMITER_URL whose
    // host cannot be resolved at boot falls back to NoopLimiter (the worker still
    // serves calls, unlimited, until restart) rather than crash-looping.
    let limiter: Arc<dyn CallLimiter> = if limiter_url.is_empty() {
        Arc::new(NoopLimiter)
    } else {
        let hostport = limiter_url
            .strip_prefix("http://")
            .unwrap_or(&limiter_url)
            .trim_end_matches('/');
        match hostport.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => {
                eprintln!("call-limiter client -> {addr} (timeout {limiter_timeout_ms}ms, refresh {limiter_refresh_sec}s)");
                Arc::new(HttpCallLimiter::new(
                    Arc::new(RealHttpNetwork::new()),
                    addr,
                    std::time::Duration::from_millis(limiter_timeout_ms),
                ))
            }
            None => {
                eprintln!("WARNING: LIMITER_URL {limiter_url:?} did not resolve; running unlimited (NoopLimiter)");
                Arc::new(NoopLimiter)
            }
        }
    };

    // CDR sink: publish to RabbitMQ when `B2BUA_CDR_RABBITMQ_URL` is set, else
    // discard (endurance default unless wired). Either way it sits behind the
    // `BufferedCdrWriter` bounded queue (drop-on-overload at `cdr_queue` depth),
    // so the in-process max buffer is identical regardless of sink.
    // Build the shared metrics registry HERE (before the CDR writers) and inject
    // the same handle into the core via `deps.metrics`, so the writers — built
    // before the core spawns — record into the registry the core exports at
    // `/metrics`. Without this the CDR writers minted private atomics that were
    // never scraped, leaving `b2bua_cdr_written_total` dead at 0.
    let metrics = B2buaMetrics::new();
    let cdr_inner: Arc<dyn CdrWriter> = match env::var("B2BUA_CDR_RABBITMQ_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let queue = env_or("B2BUA_CDR_RABBITMQ_QUEUE", "cdr");
            let max_len: i64 = env_or("B2BUA_CDR_RABBITMQ_MAX_LEN", "100000")
                .parse()
                .expect("B2BUA_CDR_RABBITMQ_MAX_LEN");
            eprintln!(
                "b2bua-runner CDR sink: RabbitMQ queue={queue:?} max_len={max_len} (buffer={cdr_queue})"
            );
            Arc::new(cdr_rabbitmq::RabbitMqCdrWriter::new(
                url,
                queue,
                max_len,
                metrics.clone(),
            ))
        }
        _ => Arc::new(NullCdrWriter),
    };
    let cdr = Arc::new(BufferedCdrWriter::spawn(cdr_inner, cdr_queue, metrics.clone()));

    let clock = Clock::system();

    // --- Replication wiring (opt-in, S11). `None` keeps the legacy path. ---
    let replication = if env_flag("B2BUA_REPL") {
        match build_membership().await {
            Some((membership, addressing)) => {
                let repl_listen = resolve(&env_or("B2BUA_REPL_LISTEN", "0.0.0.0:9092"));
                // Cluster-wide repl port peers are reached on; defaults to our
                // own listen port (homogeneous pool).
                let repl_port: u16 =
                    env_or("B2BUA_REPL_PORT", &repl_listen.port().to_string()).parse().expect("B2BUA_REPL_PORT");
                let incarnation_gen = boot_incarnation();
                let store = Arc::new(ReplicatingCallStore::new(incarnation_gen, clock.clone()));
                eprintln!(
                    "b2bua-runner replication ENABLED: listen={repl_listen} peer_port={repl_port} incarnation_gen={incarnation_gen}"
                );
                // Diagnostic: log the discovered peer set a few times so we can
                // see whether the K8sMembership informer actually populates peers
                // (it starts empty and fills async). Empty after several seconds
                // ⇒ informer/watch problem; populated ⇒ the issue is downstream.
                {
                    let m = membership.clone();
                    tokio::spawn(async move {
                        for _ in 0..6 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            let peers: Vec<String> = m
                                .snapshot()
                                .into_iter()
                                .map(|p| format!("{}@{}", p.ordinal, p.host))
                                .collect();
                            eprintln!("b2bua-runner repl membership snapshot: [{}]", peers.join(", "));
                        }
                    });
                }
                Some(ReplicationSetup {
                    network: Arc::new(RealReplicationNetwork::new()),
                    membership,
                    store,
                    listen_addr: repl_listen,
                    addr_resolver: make_addr_resolver(repl_port, addressing),
                    incarnation_gen,
                })
            }
            None => None,
        }
    } else {
        None
    };

    let store: Arc<dyn b2bua::store::CallStore> = Arc::new(InMemoryCallStore::new());
    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to_with_limiter(
            dest_host.clone(),
            dest_port,
            stress_limiter_from_env(),
        )),
        limiter,
        cdr,
        store,
        clock: clock.clone(),
        id_gen: Arc::new(IdGen::from_entropy()),
        replication,
        metrics: metrics.clone(),
    };

    // The core drives the recv loop on one boxed clone of the shared endpoint;
    // the `udp_metrics` getters read another clone live (forwarding impl on
    // `Arc<dyn UdpEndpoint>`).
    let core = Arc::new(B2buaCore::spawn(Box::new(endpoint.clone()), deps));
    // `metrics` already holds the same handle the core exports (injected via deps).

    eprintln!(
        "b2bua-runner pid={} listening UDP {local} -> routing all calls to {dest_host}:{dest_port} (resolved per-call; ordinal={ordinal}, queue={queue_max}, cdr_queue={cdr_queue})",
        std::process::id()
    );

    // Readiness/metrics probe server (shared `probe-http`). The `/ready` state is
    // a single read of one source: `core.readiness_state()` already folds in the
    // Draining latch, so there is no second flag to drift from it. Held in a
    // binding for the process lifetime (its accept loop aborts on drop). The
    // `/metrics` body concatenates this worker's metric sources per scrape.
    let _probe = {
        let core_ready = core.clone();
        let txn_metrics = core.txn_metrics().clone();
        // The worker-side overload signal (Tier-3 admission gate INPUTs +
        // DECISIONs + emergency-admit counter). Reachable from the core; its
        // `prometheus_text` is appended like the proxy-self gate's exposition.
        let overload = core.overload().clone();
        // The full `UdpTransportMetrics` shape (brake counters + live queue
        // depth/max/tail-drop), superseding the old standalone brake-only text.
        let udp_metrics = udp_metrics.clone();
        let routes = probe_http::ProbeRoutes {
            metrics: Arc::new(move || {
                let mut text = metrics.prometheus_text();
                text.push_str(&txn_metrics_text(&txn_metrics));
                text.push_str(&udp_metrics.prometheus_text());
                // Worker-side overload INPUTs + DECISIONs + emergency-admit.
                text.push_str(&overload.prometheus_text());
                // jemalloc footprint/purge/decay-config counters (see the
                // #[global_allocator] note). Same cfg as the allocator dep.
                #[cfg(not(target_env = "msvc"))]
                text.push_str(&jemalloc_stats::prometheus_text());
                text
            }),
            ready: Arc::new(move || match core_ready.readiness_state() {
                b2bua::repl::ReadinessState::Ready => probe_http::ProbeState::Ready,
                b2bua::repl::ReadinessState::Draining => probe_http::ProbeState::Draining,
                b2bua::repl::ReadinessState::NotReady => probe_http::ProbeState::NotReady,
            }),
            // /debug/heap → jemalloc heap profile (needs the profiling build +
            // _RJEM_MALLOC_CONF=prof:true). Names every live-allocation call
            // stack so the RSS leak's sources are attributed, not guessed.
            #[cfg(not(target_env = "msvc"))]
            heap: Some(Arc::new(jemalloc_stats::dump_profile)),
            #[cfg(target_env = "msvc")]
            heap: None,
        };
        match probe_http::ProbeServer::start(metrics_sa, routes).await {
            Ok(server) => {
                eprintln!("b2bua-runner metrics on http://{}/metrics (readiness /ready)", server.addr());
                Some(server)
            }
            Err(e) => {
                eprintln!("b2bua-runner metrics server failed to bind {metrics_sa}: {e}");
                None
            }
        }
    };

    // Memory-attribution sampler: push the store + replication map sizes into
    // their gauges every 5 s so an RSS climb can be pinned to a specific map
    // even when active_calls is flat (the lens the last leak hunt lacked — see
    // b2bua_store_calls vs b2bua_active_calls, and b2bua_repl_meta_backup for
    // un-reaped X11 ghost-backup copies). 5 s is well inside the scrape cadence;
    // the sample is a couple of brief locks, off the call path.
    {
        let core = core.clone();
        let clock = clock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                core.sample_gauges();
                // Physically reclaim expired backup-replica bodies + changelog
                // delete-tombstones. `reap` is correct but was never DRIVEN in
                // production (only the test harness called it), so the changelog
                // and bak: held-set grew unbounded under steady create/terminate
                // churn (repl_changelog_entries / repl_backup_held climb at the
                // terminate rate while active_calls is flat) -> monotonic RSS+CPU
                // -> eventual OOM. Same lesson as the timer wheel: logical/lazy
                // cleanup is bounded only by OOM; physical reclamation MUST be
                // actively ticked.
                if let Some(repl) = core.repl_store() {
                    // Sample inner-store map sizes BEFORE reap so a leak is visible
                    // even if reap is the thing fixing it (bodies/idx/tombstones).
                    let (bodies, idx, _meta, tomb) = repl.map_lens();
                    core.metrics().set_store_map_sizes(bodies as u64, idx as u64, tomb as u64);
                    repl.reap(clock.now_ms()).await;
                }
            }
        });
    }

    // Graceful shutdown: SIGTERM (k8s pod termination) latches Draining — OPTIONS
    // self-reports 503 and the readiness probe flips NotReady so the proxy steers
    // new calls away — then we wait the drain grace before exiting so in-flight
    // calls finish. Ctrl-C (interactive) exits immediately.
    let drain_grace_ms: u64 = env_or("B2BUA_DRAIN_GRACE_MS", "5000").parse().unwrap_or(5000);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("b2bua-runner SIGINT — shutting down");
        }
        _ = wait_sigterm() => {
            eprintln!("b2bua-runner SIGTERM — begin draining ({drain_grace_ms}ms grace)");
            // Latch Draining, then wait for the live call map to clear — capped
            // at the grace. A node with no calls exits at once; a busy node is
            // bounded; a residual is logged, never silently cut.
            let residual = core.drain(std::time::Duration::from_millis(drain_grace_ms)).await;
            if residual == 0 {
                eprintln!("b2bua-runner drained cleanly — exiting");
            } else {
                eprintln!("b2bua-runner drain grace elapsed with {residual} call(s) still active — exiting");
            }
        }
    }
    drop(core);
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
            eprintln!("b2bua-runner cannot install SIGTERM handler: {e}");
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
    use super::*;

    fn suffixes(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ip_literal_dest_is_admissible_regardless_of_suffixes() {
        // The default B2BUA_DEST (127.0.0.1) and any IP literal must boot even
        // with an empty allow-list.
        assert!(validate_runner_overrides("127.0.0.1", &suffixes(&[]), 70).is_ok());
    }

    #[test]
    fn dest_matching_a_configured_suffix_boots() {
        assert!(validate_runner_overrides(
            "sipp-uas.sip-test.svc.cluster.local",
            &suffixes(&[".svc.cluster.local"]),
            70
        )
        .is_ok());
    }

    #[test]
    fn dest_excluded_by_its_own_allow_list_refuses_boot() {
        let e = validate_runner_overrides("bob", &suffixes(&[".svc.cluster.local"]), 70)
            .expect_err("a default callee its own allow-list rejects must fail boot");
        assert!(e.contains("B2BUA_DEST"), "msg was: {e}");
    }

    #[test]
    fn star_wildcard_admits_any_dest() {
        assert!(validate_runner_overrides("bob", &suffixes(&["*"]), 70).is_ok());
    }

    #[test]
    fn tier1_pct_out_of_range_refuses_boot() {
        assert!(validate_runner_overrides("127.0.0.1", &suffixes(&["*"]), 0).is_err());
        assert!(validate_runner_overrides("127.0.0.1", &suffixes(&["*"]), 101).is_err());
        // boundaries are in range
        assert!(validate_runner_overrides("127.0.0.1", &suffixes(&["*"]), 1).is_ok());
        assert!(validate_runner_overrides("127.0.0.1", &suffixes(&["*"]), 100).is_ok());
    }
}
