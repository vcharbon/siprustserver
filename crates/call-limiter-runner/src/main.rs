//! `call-limiter-runner` — the deployed limiter process.
//!
//! Binds the [`LimiterServer`] on the real (hyper) transport, exposing
//! `/v1/{admit,release,refresh}` + `/metrics` + `/healthz` on one port, and
//! runs a periodic TTL-sweep janitor so even a fully idle server reclaims
//! expired window keys (the sweep-on-access path only fires on traffic).
//!
//! Stateless, no persistence: on restart the empty state is re-filled within
//! `~windowSec` by active calls' refresh timers, and the b2bua fails open
//! during the brief downtime. Deployed as a single replica (ClusterIP).
//!
//! ## Config (env)
//! - `LIMITER_LISTEN`                  (default `0.0.0.0:8080`)
//! - `LIMITER_WINDOW_SECONDS`          (default `300`)
//! - `LIMITER_ACTIVE_WINDOWS`          (default `3`)
//! - `LIMITER_TTL_SECONDS`             (default `1200`)
//! - `LIMITER_JANITOR_INTERVAL_SECONDS`(default = window seconds)

use std::sync::Arc;
use std::time::Duration;

use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpTransport, RealHttpNetwork};
use sip_clock::Clock;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    // Subscriber first (ADR-0026); the guard drains the log writer at exit.
    let _observe = observe::init_production("call-limiter-runner");

    let listen: String = env_or("LIMITER_LISTEN", "0.0.0.0:8080".to_string());
    let cfg = LimiterConfig {
        window_sec: env_or("LIMITER_WINDOW_SECONDS", 300),
        active_windows: env_or("LIMITER_ACTIVE_WINDOWS", 3),
        ttl_sec: env_or("LIMITER_TTL_SECONDS", 1200),
    };
    let janitor_secs: u64 = env_or(
        "LIMITER_JANITOR_INTERVAL_SECONDS",
        cfg.window_sec.max(1) as u64,
    );

    let addr = listen
        .parse()
        .unwrap_or_else(|e| panic!("bad LIMITER_LISTEN {listen:?}: {e}"));

    let store = Arc::new(WindowStore::new(cfg, Clock::system()));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));

    let net = RealHttpNetwork::new();
    let _handle = net
        .serve(addr, server)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!(
        %addr,
        window_sec = cfg.window_sec,
        active_windows = cfg.active_windows,
        ttl_sec = cfg.ttl_sec,
        janitor_sec = janitor_secs,
        "call-limiter listening"
    );

    // Periodic janitor: reclaim TTL-expired keys even with no traffic.
    let janitor_store = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(janitor_secs.max(1)));
        loop {
            tick.tick().await;
            let swept = janitor_store.sweep_now();
            if swept > 0 {
                tracing::info!(swept, "janitor swept expired keys");
            }
        }
    });

    // Run until terminated.
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!(signal = "SIGINT", "shutting down"),
        Err(e) => tracing::warn!(error = %e, "signal handler error"),
    }
}
