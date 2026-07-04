//! Standalone, containerizable B2BUA worker process.
//!
//! Wires the `b2bua` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork` — no `Recorder` decorator, no simulated
//! fabric) and a **system wall clock** (`Clock::system`, so transaction/dialog
//! timers fire). The generic runner plumbing — env grammar, bind + Tier-1
//! brake, advertise coercion, deps defaults, probe server, gauge sampler,
//! SIGTERM/drain — lives in `b2bua-runner-kit` (shared with downstream runner
//! binaries per ADR-0016); this binary keeps only its OWN composition choices:
//!   - route: `ScriptedDecisionEngine::route_all_to_with_limiter(DEST, stress)`
//!            (the HTTP call-control adapter is a deferred slice; routing all
//!            calls to a fixed UAS mirrors the k8s `worker -> sipp-uas` topology).
//!            It attaches an always-on `B2BUA_STRESS_LIMITER` entry to every call
//!            (full-chain stress) and honors an inbound `X-Api-Call` `call_limiter`
//!            array so a dedicated stream can enforce a real cap.
//!   - CDR  : RabbitMQ sink when `B2BUA_CDR_RABBITMQ_URL` is set, else the
//!            kit's discarding `NullCdrWriter` — either way behind the bounded
//!            `BufferedCdrWriter` (drop-on-overload).
//!   - HA   : opt-in peer-to-peer replication (S11) with static or kube
//!            EndpointSlice membership.
//!   - alloc: jemalloc (+ heap-profiling `/debug/heap` route, jemalloc stats
//!            appended to `/metrics`).
//!
//! Config via env (all optional; the generic `B2BUA_*`/`LIMITER_*`/`WORKER_*`
//! knobs are parsed by `b2bua_runner_kit::RunnerEnv` — see its field docs):
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
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use b2bua::cdr::CdrWriter;
use b2bua::decision::{CallLimiterEntry, ScriptedDecisionEngine};
use b2bua::repl::{PeerResolver, ReplicatingCallStore};
use b2bua::ReplicationSetup;
use b2bua_runner_kit::{env_flag, env_or, resolve, split_host_port, validate_default_dest, RunnerEnv};
use repl_net::RealReplicationNetwork;
use topology::{Membership, Peer, StaticMembership};

mod cdr_rabbitmq;

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

#[tokio::main]
async fn main() {
    // Loud confirmation the jemalloc decay config (_RJEM_MALLOC_CONF) actually
    // parsed — a typo is silently ignored. Mirrored by the jemalloc_opt_*_decay_ms
    // gauges on /metrics.
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();

    // The b-leg callee (`B2BUA_DEST`) is passed to the decision engine as an
    // UNRESOLVED host:port. A DNS name is resolved PER CALL — and round-robined
    // across a headless Service's pod set — in b2bua's `apply_route`, so the b-leg
    // goes pod-direct from the LB VIP with no kube-proxy ClusterIP NAT. Resolving
    // once here would instead pin every call to a single startup-resolved pod (and
    // could fail the worker's boot if the callee Service has no endpoints yet). An
    // IP literal passes straight through the resolver unchanged.
    let dest = env_or("B2BUA_DEST", "127.0.0.1:5070");
    let (dest_host, dest_port) = split_host_port(&dest);

    // Generic runner plumbing (b2bua-runner-kit): env grammar → bind (Tier-1
    // brake installed) → advertise coercion → validated config + metrics/clock.
    let base = RunnerEnv::from_env().bind("b2bua-runner").await;

    // Runner-only coherence (not visible to `B2buaConfig::validate`): the default
    // callee must be admissible under the worker's own allow-list — refuse to
    // boot with a clear message rather than silently 503 every call.
    validate_default_dest(&dest_host, &base.config.worker_allowed_target_suffixes)
        .unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));

    // CDR sink: publish to RabbitMQ when `B2BUA_CDR_RABBITMQ_URL` is set, else
    // the kit's discarding default. Either way it sits behind the
    // `BufferedCdrWriter` bounded queue (drop-on-overload at `cdr_queue` depth),
    // so the in-process max buffer is identical regardless of sink. The writer
    // records into `base.metrics` — the SAME registry the core exports at
    // `/metrics` (a private registry here would leave `b2bua_cdr_written_total`
    // dead at 0).
    let cdr_sink: Option<Arc<dyn CdrWriter>> = match env::var("B2BUA_CDR_RABBITMQ_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let queue = env_or("B2BUA_CDR_RABBITMQ_QUEUE", "cdr");
            let max_len: i64 = env_or("B2BUA_CDR_RABBITMQ_MAX_LEN", "100000")
                .parse()
                .expect("B2BUA_CDR_RABBITMQ_MAX_LEN");
            eprintln!(
                "b2bua-runner CDR sink: RabbitMQ queue={queue:?} max_len={max_len} (buffer={})",
                base.env.cdr_queue
            );
            Some(Arc::new(cdr_rabbitmq::RabbitMqCdrWriter::new(
                url,
                queue,
                max_len,
                base.metrics.clone(),
            )))
        }
        _ => None,
    };

    let mut deps = base.deps(
        Arc::new(ScriptedDecisionEngine::route_all_to_with_limiter(
            dest_host.clone(),
            dest_port,
            stress_limiter_from_env(),
        )),
        cdr_sink,
    );

    // --- Replication wiring (opt-in, S11). `None` keeps the legacy path. ---
    deps.replication = if env_flag("B2BUA_REPL") {
        match build_membership().await {
            Some((membership, addressing)) => {
                let repl_listen = resolve(&env_or("B2BUA_REPL_LISTEN", "0.0.0.0:9092"));
                // Cluster-wide repl port peers are reached on; defaults to our
                // own listen port (homogeneous pool).
                let repl_port: u16 =
                    env_or("B2BUA_REPL_PORT", &repl_listen.port().to_string()).parse().expect("B2BUA_REPL_PORT");
                let incarnation_gen = boot_incarnation();
                let store = Arc::new(ReplicatingCallStore::new(incarnation_gen, base.clock.clone()));
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

    // No extra ServiceDefs: the in-tree services (transfer, relay-first-18x)
    // ride `default_rules()` at runtime; `compose_services()` (lib.rs) is the
    // doc-generation registry.
    let core = base.spawn(deps, Vec::new());

    eprintln!(
        "b2bua-runner pid={} listening UDP {} -> routing all calls to {dest_host}:{dest_port} (resolved per-call; ordinal={}, queue={}, cdr_queue={})",
        std::process::id(),
        base.local,
        base.env.ordinal,
        base.env.queue_max,
        base.env.cdr_queue
    );

    // jemalloc footprint/purge/decay-config counters appended to `/metrics`,
    // and the `/debug/heap` jemalloc heap profile (needs the profiling build +
    // _RJEM_MALLOC_CONF=prof:true) so an RSS leak's sources are attributed,
    // not guessed. Same cfg as the #[global_allocator].
    #[cfg(not(target_env = "msvc"))]
    let extra_metrics: Option<probe_http::MetricsFn> =
        Some(Arc::new(jemalloc_stats::prometheus_text));
    #[cfg(target_env = "msvc")]
    let extra_metrics: Option<probe_http::MetricsFn> = None;
    #[cfg(not(target_env = "msvc"))]
    let heap: Option<probe_http::HeapDumpFn> = Some(Arc::new(jemalloc_stats::dump_profile));
    #[cfg(target_env = "msvc")]
    let heap: Option<probe_http::HeapDumpFn> = None;

    // Held in a binding for the process lifetime (accept loop aborts on drop).
    let _probe = base.spawn_probe_server(&core, extra_metrics, heap).await;

    base.spawn_gauge_sampler(&core);
    base.run_until_shutdown(&core).await;
}
