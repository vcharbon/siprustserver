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
//!     from new-dialog selection). Health writes use `NoopControl` (the static
//!     pool stays alive; band classification still applies).
//!   - `AlwaysAdmitGate` self-gate (the real ELU/CPS gate is deferred).
//!   - Prometheus `/metrics` + `/healthz` via the crate's `MetricsServer`.
//!
//! Config via env (all optional):
//!   PROXY_LISTEN     SIP listen addr                       (default 0.0.0.0:5060)
//!   PROXY_ADVERTISE  host:port stamped on Via/Record-Route (default: listen, or
//!                    127.0.0.1 if listen IP is unspecified)
//!   PROXY_WORKERS    worker pool "id@host:port,..."        (default w0@127.0.0.1:5060)
//!   PROXY_METRICS    Prometheus HTTP listen addr           (default 0.0.0.0:9090)
//!   PROXY_QUEUE      inbound UDP queue depth (packets)      (default 8192)
//!   PROXY_HMAC_KID   stickiness cookie key id              (default k0)
//!   PROXY_HMAC_KEY   stickiness cookie secret (>=16 bytes) (default dev key)
//!   HEALTH_INTERVAL_MS / HEALTH_TIMEOUT_MS / HEALTH_THRESHOLD  (probe tuning)

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::metrics_server::MetricsServer;
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::control::NoopControl;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
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

#[tokio::main]
async fn main() {
    let listen = env_or("PROXY_LISTEN", "0.0.0.0:5060");
    let workers = env_or("PROXY_WORKERS", "w0@127.0.0.1:5060");
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

    let registry: Arc<dyn WorkerRegistry> = Arc::new(
        StaticWorkerRegistry::from_string(&workers, "PROXY_WORKERS")
            .unwrap_or_else(|e| panic!("bad PROXY_WORKERS {workers:?}: {e}")),
    );
    let hmac = Arc::new(
        StaticHmacKeyProvider::new(HmacKey::new(hmac_kid, hmac_key.into_bytes()), None)
            .unwrap_or_else(|e| panic!("bad PROXY_HMAC_KEY: {e}")),
    );
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let metrics = Arc::new(ProxyMetrics::new());
    let clock = Clock::system();
    let id_gen = Arc::new(IdGen::from_entropy());

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
        Arc::new(NoopControl),
        observer,
        clock.clone(),
        id_gen.clone(),
        HealthProbeConfig {
            interval_ms: parse_u64("HEALTH_INTERVAL_MS", 1_000),
            timeout_ms: parse_u64("HEALTH_TIMEOUT_MS", 1_500),
            threshold: parse_u64("HEALTH_THRESHOLD", 2) as u32,
        },
    );

    let core = ProxyCoreBuilder::new(advertised.clone(), strategy, registry)
        .clock(clock)
        .id_gen(id_gen)
        .metrics(metrics.clone())
        .build(endpoint);

    eprintln!(
        "sip-proxy-runner pid={} listening UDP {listen_sa} advertise={}:{} workers=[{workers}] (queue={queue_max})",
        std::process::id(),
        advertised.host,
        advertised.port,
    );

    // Prometheus endpoint. NOTE: `MetricsServer` aborts its listener task on
    // Drop, so the handle must stay live for the whole process lifetime.
    let _metrics_server = match MetricsServer::start(metrics_sa, metrics).await {
        Ok(s) => {
            eprintln!("sip-proxy-runner metrics on http://{}/metrics", s.addr());
            Some(s)
        }
        Err(e) => {
            eprintln!("sip-proxy-runner metrics server failed to bind {metrics_sa}: {e}");
            None
        }
    };

    tokio::spawn(probe.run());
    tokio::spawn(core.run());

    tokio::signal::ctrl_c()
        .await
        .expect("install ctrl-c handler");
    eprintln!("sip-proxy-runner shutting down");
    drop(_metrics_server);
}
