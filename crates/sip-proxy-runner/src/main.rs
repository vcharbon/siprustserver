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
//!   - `AlwaysAdmitGate` self-gate (the real ELU/CPS gate is deferred).
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
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let metrics = Arc::new(ProxyMetrics::new());
    let clock = Clock::system();
    let id_gen = Arc::new(IdGen::from_entropy());

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
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        health_control,
        observer,
        clock.clone(),
        id_gen.clone(),
        HealthProbeConfig {
            interval_ms: parse_u64("HEALTH_INTERVAL_MS", 1_000),
            timeout_ms: parse_u64("HEALTH_TIMEOUT_MS", 1_500),
            threshold: parse_u64("HEALTH_THRESHOLD", 2) as u32,
        },
    );

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
    let ready: probe_http::ReadyFn = {
        let reg = registry.clone();
        Arc::new(move || {
            if reg.snapshot().iter().any(|w| w.health == WorkerHealth::Alive) {
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

    // Prometheus + readiness endpoint (shared `probe-http`). NOTE: `ProbeServer`
    // aborts its accept loop on Drop, so the handle must stay live for the whole
    // process lifetime. The `/metrics` body appends the jemalloc mallctl
    // exposition (footprint/purge/decay config) per scrape — same cfg as the
    // allocator dep.
    let routes = probe_http::ProbeRoutes {
        metrics: Arc::new(move || {
            let mut t = metrics.prometheus_text();
            #[cfg(not(target_env = "msvc"))]
            t.push_str(&jemalloc_stats::prometheus_text());
            t
        }),
        ready,
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

    // Supervise every data-path task: a panicked/exited recv loop used to
    // leave the process alive with /healthz green — k8s never restarted the
    // pod and every datagram was silently black-holed. Exiting non-zero makes
    // the container restart the moment ANY recv shard or the probe dies.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("sip-proxy-runner shutting down");
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
