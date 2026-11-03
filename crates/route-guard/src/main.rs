//! `route-guard` — the node route guard beside a BGP-announced front proxy
//! (see the library's doc). Configuration: [`route_guard::config`]. It needs
//! the node's network namespace and `CAP_NET_ADMIN`, and nothing else.
//! `route-guard --withdraw` writes the withdrawing ruleset and exits: the
//! pod's init step, run before the proxy starts. `route-guard --check` writes
//! the ruleset under another table and deletes it: a deploy's smoke.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::process::ExitCode;
use std::sync::Arc;

use probe_http::{ProbeRoutes, ProbeServer};
use route_guard::config::GuardConfig;
use route_guard::fitness::Probe;
use route_guard::metrics::GuardMetrics;
use route_guard::nft::{Nft, Writer};
use route_guard::rules::{delete, ruleset, ruleset_in, CHECK_TABLE};
use route_guard::run::{readiness, run, Pulse, ONE_TIME_WRITE_BOUND};

/// The argument of the one-shot run that writes the withdrawing ruleset and
/// exits: the pod's init step, so a node whose pod starts is held out of the
/// routers' ECMP until its guard reads the proxy fit for the hold.
const WITHDRAW_ARG: &str = "--withdraw";

/// The argument of the one-shot run that writes the guard's ruleset under
/// another table, deletes it and exits: a deploy's proof that this image's
/// nft can write on the node.
const CHECK_ARG: &str = "--check";

/// How many dead bounds the loop may miss before `/healthz` fails.
const LIVENESS_DEAD_BOUNDS: u32 = 5;

fn main() -> ExitCode {
    match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(runtime) => runtime.block_on(serve()),
        Err(e) => {
            eprintln!("route-guard: the tokio runtime did not build: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The guard from start to exit, its log writer drained before the exit.
async fn serve() -> ExitCode {
    // Subscriber first (ADR-0026); the guard drains the log writer at exit.
    let observe_guard = match observe::init_production("route-guard") {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("route-guard FATAL: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = match guard().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(refusal) => {
            tracing::error!(error = %refusal, "route guard stopped");
            ExitCode::FAILURE
        }
    };
    drop(observe_guard);
    code
}

async fn guard() -> Result<(), String> {
    let cfg = GuardConfig::from_env().map_err(|errors| errors.join("; "))?;
    let writer = Nft { program: cfg.nft.clone() };
    match std::env::args().nth(1).as_deref() {
        Some(WITHDRAW_ARG) => return withdraw(&cfg, &writer).await,
        Some(CHECK_ARG) => return check(&cfg, &writer).await,
        _ => {}
    }
    let terminate = terminate()?;
    let metrics = Arc::new(GuardMetrics::starting());
    let pulse = Pulse::new();
    let live_bound = cfg.timing.dead.saturating_mul(LIVENESS_DEAD_BOUNDS);
    let routes = ProbeRoutes {
        metrics: {
            let metrics = metrics.clone();
            Arc::new(move || metrics.body())
        },
        ready: {
            let metrics = metrics.clone();
            Arc::new(move || readiness(&metrics))
        },
        heap: None,
        fit: None,
    };
    let live = {
        let pulse = pulse.clone();
        Arc::new(move || pulse.within(live_bound))
    };
    let _server = ProbeServer::start_live(cfg.metrics, routes, live)
        .await
        .map_err(|e| format!("the metrics server on {} did not start: {e}", cfg.metrics))?;
    let source = Probe { addr: cfg.fitness, bound: cfg.poll };
    run(&cfg, &source, &writer, &metrics, &pulse, terminate).await
}

/// Write the withdrawing ruleset once.
async fn withdraw(cfg: &GuardConfig, writer: &Nft) -> Result<(), String> {
    writer
        .write(ruleset(&cfg.peers, None), ONE_TIME_WRITE_BOUND)
        .await
        .map_err(|e| format!("the withdrawing ruleset was not applied: {e}"))?;
    tracing::info!(
        peers = cfg.peers.len(),
        "node held withdrawn until the route guard lets it out"
    );
    Ok(())
}

/// Write the guard's ruleset under [`CHECK_TABLE`], then delete it.
async fn check(cfg: &GuardConfig, writer: &Nft) -> Result<(), String> {
    writer
        .write(ruleset_in(CHECK_TABLE, &cfg.peers, Some(cfg.timing.dead)), ONE_TIME_WRITE_BOUND)
        .await
        .map_err(|e| format!("the check ruleset was not applied: {e}"))?;
    writer
        .write(delete(CHECK_TABLE), ONE_TIME_WRITE_BOUND)
        .await
        .map_err(|e| format!("the check table was not deleted: {e}"))?;
    tracing::info!(table = CHECK_TABLE, "nft writes and deletes a route guard table here");
    Ok(())
}

/// SIGTERM or SIGINT.
fn terminate() -> Result<impl std::future::Future<Output = ()>, String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())
        .map_err(|e| format!("cannot install the SIGTERM handler: {e}"))?;
    let mut int = signal(SignalKind::interrupt())
        .map_err(|e| format!("cannot install the SIGINT handler: {e}"))?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}
