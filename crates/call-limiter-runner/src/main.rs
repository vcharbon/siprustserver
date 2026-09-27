//! `call-limiter-runner` — the deployed limiter process.
//!
//! Binds the [`LimiterServer`] on the real (hyper) transport, exposing
//! `/v1/{admit,release,refresh,health}` + `/metrics` + `/healthz` on one port, and
//! runs a periodic janitor so even a fully idle server drops the sets whose
//! lease lapsed (the sweep-on-access path only fires on traffic).
//!
//! Stateless, no persistence: on restart the store is empty; the counted calls
//! re-register their sets on their next refresh (ADR-0038). The b2bua fails
//! open during the downtime. Deployed as a single replica (ClusterIP).
//!
//! ## Config (env)
//! - `LIMITER_LISTEN`                   (default `0.0.0.0:8080`)
//! - `LIMITER_LEASE_SECONDS`            (default `120`; the workers refresh
//!   every `LIMITER_REFRESH_SECONDS`, below it)
//! - `LIMITER_JANITOR_INTERVAL_SECONDS` (default `10`)

use std::sync::Arc;
use std::time::Duration;

use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpTransport, RealHttpNetwork};
use sip_clock::Clock;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() {
    // Subscriber first (ADR-0026); the guard drains the log writer at exit.
    let _observe = observe::init_production("call-limiter-runner");

    let listen: String = env_or("LIMITER_LISTEN", "0.0.0.0:8080".to_string());
    let cfg = LimiterConfig { lease_sec: env_or("LIMITER_LEASE_SECONDS", 120) };
    let janitor_secs: u64 = env_or("LIMITER_JANITOR_INTERVAL_SECONDS", 10);

    let addr = listen.parse().unwrap_or_else(|e| panic!("bad LIMITER_LISTEN {listen:?}: {e}"));

    let store = Arc::new(CallStore::new(cfg, Clock::system()));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));

    let net = RealHttpNetwork::new();
    let _handle =
        net.serve(addr, server).await.unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!(
        %addr,
        lease_sec = cfg.lease_sec,
        janitor_sec = janitor_secs,
        "call-limiter listening"
    );

    // Periodic janitor: drop lapsed sets even with no traffic.
    let janitor_store = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(janitor_secs.max(1)));
        loop {
            tick.tick().await;
            let lapsed = janitor_store.sweep_now();
            if lapsed > 0 {
                tracing::info!(lapsed, "janitor dropped sets whose lease lapsed");
            }
        }
    });

    // Run until terminated.
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!(signal = "SIGINT", "shutting down"),
        Err(e) => tracing::warn!(error = %e, "signal handler error"),
    }
}
