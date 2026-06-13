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

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::{CallLimiterEntry, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{PeerResolver, ReplicatingCallStore};
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use call::Call;
use http_net::RealHttpNetwork;
use repl_net::RealReplicationNetwork;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_txn::IdGen;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

/// Minimal Prometheus exposition + probe server. Hand-rolled (no HTTP framework
/// dep), mirroring `sip-proxy`'s MetricsServer.
///   - `GET /metrics`  → Prometheus text.
///   - `GET /healthz`  → liveness: `200 ok` while the process runs.
///   - `GET /ready`    → readiness: `200 ready` iff the worker is replication-
///                       ready (re-hydrated + backup-current) **and** not
///                       draining; `503` otherwise. The k8s readinessProbe hits
///                       this so the proxy's EndpointSlice view respects
///                       `NotReady`/`Draining` (ADR-0011 X6), matching the
///                       worker's own OPTIONS self-report.
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

async fn serve_metrics(
    addr: SocketAddr,
    metrics: B2buaMetrics,
    txn_metrics: sip_txn::TransactionMetrics,
    ready: Arc<dyn Fn() -> bool + Send + Sync>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("b2bua-runner metrics on http://{}/metrics", listener.local_addr()?);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        let txn_metrics = txn_metrics.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            // On-demand CPU flamegraph (pprof SIGPROF sampling -> inferno SVG).
            // Internal debug route on the metrics port — NOT the SIP datapath.
            // Blocks for the sample window on a blocking thread (the worker's
            // async runtime keeps serving SIP); the profiler is built only for
            // the request. `?seconds=N` (default 20, max 120).
            let target = req.split_whitespace().nth(1).unwrap_or("/");
            if target.split('?').next() == Some("/debug/flamegraph") {
                let secs = flamegraph_util::parse_seconds(target, 20, 120);
                let result =
                    tokio::task::spawn_blocking(move || flamegraph_util::capture_svg(secs, 99))
                        .await
                        .unwrap_or_else(|e| Err(format!("join error: {e}")));
                match result {
                    Ok(svg) => {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            svg.len()
                        );
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(&svg).await;
                    }
                    Err(e) => {
                        let body = format!("flamegraph failed: {e}\n");
                        let header = format!(
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(body.as_bytes()).await;
                    }
                }
                return;
            }
            let (status, body) = if req.starts_with("GET /metrics") {
                let mut text = metrics.prometheus_text();
                text.push_str(&txn_metrics_text(&txn_metrics));
                ("200 OK", text)
            } else if req.starts_with("GET /healthz") {
                ("200 OK", "ok\n".to_string())
            } else if req.starts_with("GET /ready") {
                if ready() {
                    ("200 OK", "ready\n".to_string())
                } else {
                    ("503 Service Unavailable", "not-ready\n".to_string())
                }
            } else {
                ("404 Not Found", String::new())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

#[tokio::main]
async fn main() {
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

    // Real, non-recording transport: a plain tokio UDP socket.
    let net = RealSignalingNetwork::new();
    let endpoint = net
        .bind_udp(BindUdpOpts::new(listen_sa, queue_max))
        .await
        .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"));
    let local = endpoint.local_addr();

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

    let config = B2buaConfig {
        self_ordinal: ordinal.clone(),
        sip_local_ip: advertise_ip,
        sip_local_port: advertise_port,
        b2b_outbound_proxy,
        cdr_buffer_queue_max: cdr_queue,
        event_dispatch_concurrency: concurrency,
        per_call_queue_cap: call_cap,
        keepalive_interval_sec: keepalive_sec,
        keepalive_timeout_sec,
        reboot_budget_sec,
        limiter_refresh_sec,
        setup_timeout_sec,
        ..Default::default()
    };
    // Forbid booting with a config that would silently break HA: too-short a
    // keepalive, or a reboot budget that cannot outlast a primary reboot / a
    // keepalive refresh gap (which would self-evict healthy backups).
    config
        .validate()
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

    let core = Arc::new(B2buaCore::spawn(endpoint, deps));
    // `metrics` already holds the same handle the core exports (injected via deps).

    eprintln!(
        "b2bua-runner pid={} listening UDP {local} -> routing all calls to {dest_host}:{dest_port} (resolved per-call; ordinal={ordinal}, queue={queue_max}, cdr_queue={cdr_queue})",
        std::process::id()
    );

    // Readiness probe state: a node reports NotReady while re-hydrating and once
    // SIGTERM latches `draining`. The metrics server reads both.
    let draining = Arc::new(AtomicBool::new(false));
    {
        let core = core.clone();
        let txn_metrics = core.txn_metrics().clone();
        let draining = draining.clone();
        let ready: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || core.is_ready() && !draining.load(Ordering::Relaxed));
        tokio::spawn(async move {
            if let Err(e) = serve_metrics(metrics_sa, metrics, txn_metrics, ready).await {
                eprintln!("b2bua-runner metrics server error: {e}");
            }
        });
    }

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
            core.begin_draining();
            draining.store(true, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(drain_grace_ms)).await;
            eprintln!("b2bua-runner drain grace elapsed — exiting");
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
