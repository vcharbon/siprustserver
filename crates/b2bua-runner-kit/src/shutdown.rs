//! How the worker process stops: what ends its service, and the exit it takes.
//!
//! - **Ctrl-C** (interactive): Draining, then no wait for the live calls,
//!   only the limiter release flush, which a second Ctrl-C cuts short. Exit 0.
//! - **SIGTERM**, or the worker's **own endpoint withdrawn** with no SIGTERM
//!   (ADR-0031 D5, D6): the same D2 drain — Draining, then the first of the
//!   live calls clearing, a withdrawn worker's backups holding them past
//!   `B2BUA_DRAIN_MIN_MS`, or `B2BUA_DRAIN_GRACE_MS`; the queued limiter
//!   releases flushed within `B2BUA_DRAIN_RELEASE_FLUSH_MS` (ADR-0040 decision
//!   9). Exit 0; the orchestrator restarts the ordinal.
//! - **A loop task ended** — the transaction owner or the router returned,
//!   was aborted or panicked, before or during a drain: nothing serves any
//!   more. The queued limiter releases are flushed within the same bound,
//!   and the process exits non-zero; the orchestrator restarts it.
//!
//! Every exit then closes the CDR sink within `B2BUA_DRAIN_CDR_FLUSH_MS`, which
//! no signal cuts short: the records of the calls that ended are delivered or
//! counted dropped, never left in a queue the exit discards. The close logs
//! the records the sink counted written (a broker sink counts them) and
//! dropped.

use std::future::Future;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use b2bua::drain::{DrainBounds, DrainExit};
use b2bua::B2buaCore;
use sip_clock::wall::millis_u64;

use crate::RunnerBase;

/// What ended the worker's service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Interrupt,
    Terminate,
    Withdrawn,
    LoopEnded(&'static str),
}

/// The first of the stops to happen; a loop that ended wins a tie.
pub(crate) async fn next_stop(
    interrupt: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
    withdrawn: impl Future<Output = ()>,
    loop_ended: impl Future<Output = &'static str>,
) -> Stop {
    tokio::select! {
        biased;
        name = loop_ended => Stop::LoopEnded(name),
        _ = interrupt => Stop::Interrupt,
        _ = terminate => Stop::Terminate,
        _ = withdrawn => Stop::Withdrawn,
    }
}

impl RunnerBase {
    /// Serve until a stop (see the module doc), act on it, and return the
    /// process's exit code.
    pub async fn run_until_shutdown(&self, core: &Arc<B2buaCore>) -> ExitCode {
        let interrupt = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let terminate = wait_sigterm(&self.name);
        serve_until_stop(core, &self.name, self.env.drain, interrupt, terminate).await
    }
}

/// Serve `core` until `interrupt`, `terminate`, its own withdrawal or a loop
/// task ending, act on it under `bounds`, and return the exit code.
pub(crate) async fn serve_until_stop(
    core: &B2buaCore,
    name: &str,
    bounds: DrainBounds,
    interrupt: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
) -> ExitCode {
    let loops = core.loops().clone();
    let stop = next_stop(interrupt, terminate, core.withdrawn(), loops.wait_ended()).await;
    match stop {
        Stop::Interrupt => interrupted(core, name, bounds).await,
        Stop::Terminate | Stop::Withdrawn => drained(core, name, bounds, stop).await,
        Stop::LoopEnded(ended) => loop_ended(core, name, bounds, ended).await,
    }
}

/// Ctrl-C: Draining first, so no new call is admitted while the queued
/// releases leave; a second Ctrl-C gives them up at once.
async fn interrupted(core: &B2buaCore, name: &str, bounds: DrainBounds) -> ExitCode {
    let release_flush_ms = millis_u64(bounds.release_flush);
    tracing::info!(service = name, signal = "SIGINT", release_flush_ms, "shutting down");
    core.begin_draining();
    tokio::select! {
        _ = core.limiter().flush(bounds.release_flush) => {}
        _ = tokio::signal::ctrl_c() => {
            let given_up = core.limiter().give_up_all();
            tracing::warn!(service = name, given_up, "second SIGINT: release flush cut short");
        }
    }
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::SUCCESS
}

/// SIGTERM or the worker's own withdrawal: the D2 drain, the loops still
/// watched — one ending mid-drain cuts it short and exits non-zero. The exit
/// reason and the residual are logged, never silently cut.
async fn drained(core: &B2buaCore, name: &str, bounds: DrainBounds, stop: Stop) -> ExitCode {
    let cause = match stop {
        Stop::Withdrawn => "own endpoint withdrawn",
        _ => "SIGTERM",
    };
    tracing::info!(
        service = name,
        cause,
        drain_grace_ms = millis_u64(bounds.grace),
        drain_min_ms = millis_u64(bounds.floor),
        release_flush_ms = millis_u64(bounds.release_flush),
        "begin draining"
    );
    let loops = core.loops().clone();
    let out = tokio::select! {
        out = core.drain(bounds) => out,
        ended = loops.wait_ended() => return loop_ended(core, name, bounds, ended).await,
    };
    let (reason, residual) = (out.exit.label(), out.residual);
    match out.exit {
        DrainExit::Quiescent | DrainExit::CaughtUp => {
            tracing::info!(service = name, reason, residual, "drained — exiting");
        }
        DrainExit::Grace | DrainExit::GracePeersBehind => {
            tracing::warn!(
                service = name,
                reason,
                residual,
                "drain grace elapsed with calls still active — exiting"
            );
        }
    }
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::SUCCESS
}

/// A loop task ended: nothing serves any more. The queued limiter releases
/// still leave, within the drain's flush bound (ADR-0040 decision 9: the
/// release queue runs on its own task), then the process exits non-zero.
async fn loop_ended(
    core: &B2buaCore,
    name: &str,
    bounds: DrainBounds,
    ended: &'static str,
) -> ExitCode {
    tracing::error!(service = name, ended, "a worker loop task ended — nothing serves any more");
    core.begin_draining();
    let flush = core.limiter().flush(bounds.release_flush).await;
    core.metrics().limiter().count_release_flush(&flush);
    tracing::error!(
        service = name,
        ended,
        releases_queued = flush.queued,
        releases_given_up = flush.given_up,
        "exiting non-zero"
    );
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::FAILURE
}

/// Close the CDR sink within `bound` and log what the close delivered and
/// dropped.
async fn close_cdr(core: &B2buaCore, name: &str, bound: Duration) {
    let metrics = core.metrics();
    let (written, dropped) = (metrics.cdr_written_total(), metrics.cdr_dropped_total());
    let started = tokio::time::Instant::now();
    core.cdr().close(started + bound).await;
    let cdr_written = metrics.cdr_written_total() - written;
    let cdr_dropped = metrics.cdr_dropped_total() - dropped;
    let elapsed_ms = millis_u64(started.elapsed());
    if cdr_dropped == 0 {
        tracing::info!(service = name, cdr_written, elapsed_ms, "CDR sink closed");
    } else {
        tracing::warn!(service = name, cdr_written, cdr_dropped, elapsed_ms, "CDR sink closed");
    }
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
            tracing::warn!(service = name, error = %e, "cannot install SIGTERM handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm(_name: &str) {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests;
