//! How the worker process stops: what ends its service, and the exit it takes.
//!
//! - **Ctrl-C** (interactive): Draining, then only the limiter release flush,
//!   which a second Ctrl-C cuts short. Exit 0.
//! - **SIGTERM**, or the worker's **own endpoint withdrawn** with no SIGTERM
//!   (ADR-0031 D5, D6): the same D2 drain — Draining, then the first of the
//!   live calls clearing, a withdrawn worker's backups holding them past
//!   `B2BUA_DRAIN_MIN_MS`, or `B2BUA_DRAIN_GRACE_MS`; the queued limiter
//!   releases flushed within `B2BUA_DRAIN_RELEASE_FLUSH_MS` (ADR-0040 decision
//!   9). Exit 0; the orchestrator restarts the ordinal.
//! - **A loop task ended** — the transaction owner or the router returned,
//!   was aborted or panicked: nothing serves any more, so the process exits
//!   non-zero at once, and the orchestrator restarts it.

use std::future::Future;
use std::process::ExitCode;
use std::sync::Arc;

use b2bua::drain::DrainExit;
use b2bua::B2buaCore;

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
        let name = &self.name;
        let interrupt = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let loops = core.loops();
        let stop =
            next_stop(interrupt, wait_sigterm(name), core.withdrawn(), loops.wait_ended()).await;
        match stop {
            Stop::Interrupt => self.interrupted(core).await,
            Stop::Terminate | Stop::Withdrawn => self.drained(core, stop).await,
            Stop::LoopEnded(ended) => {
                tracing::error!(
                    service = name,
                    ended,
                    "a worker loop task ended — nothing serves any more; exiting"
                );
                ExitCode::FAILURE
            }
        }
    }

    /// Ctrl-C: Draining first, so no new call is admitted while the queued
    /// releases leave; a second Ctrl-C gives them up at once.
    async fn interrupted(&self, core: &B2buaCore) -> ExitCode {
        let name = &self.name;
        let bounds = self.env.drain;
        let release_flush_ms = bounds.release_flush.as_millis() as u64;
        tracing::info!(service = name, signal = "SIGINT", release_flush_ms, "shutting down");
        core.begin_draining();
        tokio::select! {
            _ = core.limiter().flush(bounds.release_flush) => {}
            _ = tokio::signal::ctrl_c() => {
                let given_up = core.limiter().give_up_all();
                tracing::warn!(service = name, given_up, "second SIGINT: release flush cut short");
            }
        }
        ExitCode::SUCCESS
    }

    /// SIGTERM or the worker's own withdrawal: the D2 drain. The exit reason
    /// and the residual are logged, never silently cut.
    async fn drained(&self, core: &B2buaCore, stop: Stop) -> ExitCode {
        let name = &self.name;
        let bounds = self.env.drain;
        let cause = match stop {
            Stop::Withdrawn => "own endpoint withdrawn",
            _ => "SIGTERM",
        };
        tracing::info!(
            service = name,
            cause,
            drain_grace_ms = bounds.grace.as_millis() as u64,
            drain_min_ms = bounds.floor.as_millis() as u64,
            release_flush_ms = bounds.release_flush.as_millis() as u64,
            "begin draining"
        );
        let out = core.drain(bounds).await;
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
        ExitCode::SUCCESS
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
mod tests {
    use std::future::{pending, ready};

    use super::*;

    #[tokio::test]
    async fn a_withdrawal_with_no_signal_is_a_stop() {
        let stop = next_stop(pending(), pending(), ready(()), pending()).await;
        assert_eq!(stop, Stop::Withdrawn);
    }

    #[tokio::test]
    async fn a_loop_that_ended_wins_over_a_signal() {
        let stop = next_stop(pending(), ready(()), pending(), ready("router")).await;
        assert_eq!(stop, Stop::LoopEnded("router"));
    }

    #[tokio::test]
    async fn sigterm_and_interrupt_are_stops() {
        assert_eq!(next_stop(pending(), ready(()), pending(), pending()).await, Stop::Terminate);
        assert_eq!(next_stop(ready(()), pending(), pending(), pending()).await, Stop::Interrupt);
    }
}
