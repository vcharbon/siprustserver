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
//! - **Superseded** — the membership binds the worker's ordinal to another
//!   worker identity (ADR-0055 §6): the worker has stopped authoring, so no
//!   drain and no limiter release flush; the CDR sink delivers what was
//!   written before. Exit non-zero.
//! - **A loop task ended** — the transaction owner, the router or a vital
//!   task (the timer driver, the CDR drainer, the changelog server)
//!   returned, was aborted or panicked, before or during a
//!   drain: nothing serves any more. The queued limiter releases are flushed within the same bound,
//!   and the process exits non-zero; the orchestrator restarts it.
//! - **A loop stayed stalled** past `B2BUA_LOOP_EXIT_MS`, before or during a
//!   drain: the same exit. The flush and the CDR close run on their own tasks
//!   and their own bounds, and the runtime lets go of a thread the wedged loop
//!   holds (`runtime`), so the wedge cannot hold the exit (ADR-0033 §4).
//! - **A poll stuck on the CPU** past `B2BUA_STUCK_POLL_CPU_MS`, before any
//!   other stop or while another stop's exit runs (ADR-0057 §6): the stuck
//!   exit of [`crate::stuck`] takes over, and the process exits with the
//!   watchdog's code.
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
use b2bua::loops::LoopOut;
use b2bua::B2buaCore;
use poll_watchdog::{Stuck, Watchdog};

use sip_clock::wall::millis_u64;
use stuck::stuck_exit;

use crate::RunnerBase;

/// What ended the worker's service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    Stuck(Stuck),
    Superseded,
    Interrupt,
    Terminate,
    Withdrawn,
    Loop(LoopOut),
}

/// The first of the stops to happen. A stuck poll wins a tie, since the
/// process ends whatever else happens; a supersession wins over the rest,
/// since its fence ends the loops; a loop gone wins over the signals.
pub(crate) async fn next_stop(
    stuck: impl Future<Output = Stuck>,
    superseded: impl Future<Output = ()>,
    interrupt: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
    withdrawn: impl Future<Output = ()>,
    loop_out: impl Future<Output = LoopOut>,
) -> Stop {
    tokio::select! {
        biased;
        stuck = stuck => Stop::Stuck(stuck),
        _ = superseded => Stop::Superseded,
        out = loop_out => Stop::Loop(out),
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
        let exit = self.env.loop_exit;
        let (bounds, watchdog) = (self.env.drain, Watchdog::process());
        serve_until_stop(core, &self.name, bounds, exit, watchdog, interrupt, terminate).await
    }
}

/// Serve `core` until `interrupt`, `terminate`, its own withdrawal, its
/// supersession, a loop task ending, a loop stalled past `loop_exit` or a
/// poll `watchdog` reads stuck, act on it under `bounds`, and return the exit
/// code. A poll read stuck while another stop's exit runs cuts that exit
/// short for the stuck exit.
pub(crate) async fn serve_until_stop(
    core: &B2buaCore,
    name: &str,
    bounds: DrainBounds,
    loop_exit: Duration,
    watchdog: &Watchdog,
    interrupt: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
) -> ExitCode {
    let loops = core.loops().clone();
    let gone = loops.wait_out(loop_exit);
    let stuck = watchdog.stuck();
    let withdrawn = core.withdrawn();
    let stop = next_stop(stuck, core.superseded(), interrupt, terminate, withdrawn, gone).await;
    let stuck_first = matches!(stop, Stop::Stuck(_));
    let exit = async {
        match stop {
            Stop::Stuck(stuck) => stuck_exit(core, name, bounds, stuck).await,
            Stop::Superseded => superseded(core, name, bounds).await,
            Stop::Interrupt => interrupted(core, name, bounds).await,
            Stop::Terminate | Stop::Withdrawn => drained(core, name, bounds, loop_exit, stop).await,
            Stop::Loop(out) => loop_gone(core, name, bounds, out).await,
        }
    };
    if stuck_first {
        return exit.await;
    }
    tokio::select! {
        code = exit => code,
        stuck = watchdog.stuck() => stuck_exit(core, name, bounds, stuck).await,
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
/// watched — one ending or staying stalled mid-drain cuts it short and exits
/// non-zero. The exit reason and the residual are logged, never silently cut.
async fn drained(
    core: &B2buaCore,
    name: &str,
    bounds: DrainBounds,
    loop_exit: Duration,
    stop: Stop,
) -> ExitCode {
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
        out = loops.wait_out(loop_exit) => return loop_gone(core, name, bounds, out).await,
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

/// The worker is superseded (ADR-0055 §6): its authoring already stopped, so
/// nothing is drained and no queued limiter release leaves; the CDR sink
/// delivers the records written before, and the process exits non-zero.
async fn superseded(core: &B2buaCore, name: &str, bounds: DrainBounds) -> ExitCode {
    tracing::error!(
        service = name,
        "own ordinal bound to another worker identity: superseded, exiting without a drain"
    );
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::FAILURE
}

/// A loop or vital task ended, or a loop stayed stalled
/// ([`b2bua::loops::WorkerLoops`]): nothing serves any more. The queued limiter
/// releases still leave, within the drain's flush bound (ADR-0040 decision 9:
/// the release queue runs on its own task), then the process exits non-zero.
async fn loop_gone(core: &B2buaCore, name: &str, bounds: DrainBounds, out: LoopOut) -> ExitCode {
    let gone = match out {
        LoopOut::Ended(ended) => {
            tracing::error!(service = name, ended, "a worker loop task ended: nothing serves");
            ended
        }
        LoopOut::Stalled(stalled) => {
            tracing::error!(service = name, stalled, "a worker loop stays stalled: exiting");
            stalled
        }
    };
    core.begin_draining();
    let flush = core.limiter().flush(bounds.release_flush).await;
    core.metrics().limiter().count_release_flush(&flush);
    tracing::error!(
        service = name,
        gone,
        releases_queued = flush.queued,
        releases_given_up = flush.given_up,
        "exiting non-zero"
    );
    close_cdr(core, name, bounds.cdr_flush).await;
    ExitCode::FAILURE
}

/// Close the CDR sink within `bound` and log what the close delivered and
/// dropped.
pub(crate) async fn close_cdr(core: &B2buaCore, name: &str, bound: Duration) {
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

pub(crate) mod stuck;
#[cfg(test)]
mod tests;
