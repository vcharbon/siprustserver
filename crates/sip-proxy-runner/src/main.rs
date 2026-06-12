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
//!   - Prometheus `/metrics` + `/healthz` via the crate's `MetricsServer`.
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
//!   PROXY_HMAC_KID   stickiness cookie key id              (default k0)
//!   PROXY_HMAC_KEY   stickiness cookie secret (>=16 bytes) (default dev key)
//!   HEALTH_INTERVAL_MS / HEALTH_TIMEOUT_MS / HEALTH_THRESHOLD  (probe tuning)

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::metrics_server::MetricsServer;
use sip_proxy::observability::metrics_server::ReadinessFn;
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
        let reg = StaticWorkerRegistry::from_string(workers, "PROXY_WORKERS")
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

    let net = RealSignalingNetwork::new();

    // Main signaling endpoint.
    let endpoint = net
        .bind_udp(BindUdpOpts::new(listen_sa, queue_max))
        .await
        .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"));

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

    let core = ProxyCoreBuilder::new(advertised.clone(), strategy, registry.clone())
        .clock(clock)
        .id_gen(id_gen)
        .metrics(metrics.clone())
        .build(endpoint);

    eprintln!(
        "sip-proxy-runner pid={} listening UDP {listen_sa} advertise={}:{} workers=[{}] (queue={queue_max})",
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
    let ready: ReadinessFn = {
        let reg = registry.clone();
        Arc::new(move || reg.snapshot().iter().any(|w| w.health == WorkerHealth::Alive))
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

    // Prometheus endpoint. NOTE: `MetricsServer` aborts its listener task on
    // Drop, so the handle must stay live for the whole process lifetime.
    let _metrics_server = match MetricsServer::start_with_readiness(metrics_sa, metrics, ready).await {
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
    let core_task = tokio::spawn(core.run());

    // Supervise the two data-path tasks: a panicked/exited recv loop used to
    // leave the process alive with /healthz green — k8s never restarted the
    // pod and every datagram was silently black-holed. Exiting non-zero makes
    // the container restart the moment either task dies.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("sip-proxy-runner shutting down");
        }
        res = core_task => {
            eprintln!("sip-proxy-runner FATAL: SIP data path exited ({res:?}) — exiting for restart");
            std::process::exit(1);
        }
        res = probe_task => {
            eprintln!("sip-proxy-runner FATAL: health probe exited ({res:?}) — exiting for restart");
            std::process::exit(1);
        }
    }
    drop(_metrics_server);
}
