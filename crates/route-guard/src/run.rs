//! The guard's loop: one fitness reading per poll, the switch's step written
//! through the [`Writer`], one log line and one metric per transition.
//!
//! It starts by replacing its table from its first reading, so no drop an
//! earlier guard left survives the start, and refuses to run when that first
//! write fails. On termination it keeps guarding until the proxy stops
//! answering, then exits without a write: the last refresh runs out within
//! the dead bound and the node is withdrawn, its proxy being gone. Its own
//! `/readyz` is Ready exactly while the node is let out, so the pod is Ready
//! only once its node may carry traffic ([`readiness`]).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use probe_http::ProbeState;
use tokio::time::{Instant, MissedTickBehavior};

use crate::config::GuardConfig;
use crate::fitness::Source;
use crate::metrics::GuardMetrics;
use crate::nft::Writer;
use crate::rules::ruleset;
use crate::switch::{Cause, Reading, Switch, Write};

/// The bound of a one-time write: the start's ruleset, and the pod's init
/// step's. A refresh in the loop is bounded by half the dead bound.
pub const ONE_TIME_WRITE_BOUND: Duration = Duration::from_secs(5);

/// The guard's `/readyz`: Ready while its node is let out, NotReady while it
/// reads it withdrawn, before its first write included.
pub fn readiness(metrics: &GuardMetrics) -> ProbeState {
    if metrics.withdrawn() {
        ProbeState::NotReady
    } else {
        ProbeState::Ready
    }
}

/// The instant of the loop's last turn, for the guard's own liveness.
#[derive(Debug)]
pub struct Pulse {
    origin: Instant,
    last_ms: AtomicU64,
}

impl Pulse {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { origin: Instant::now(), last_ms: AtomicU64::new(0) })
    }

    fn beat(&self) {
        let ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// Whether the loop turned within `bound`.
    pub fn within(&self, bound: Duration) -> bool {
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        let bound = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        now.saturating_sub(self.last_ms.load(Ordering::Relaxed)) <= bound
    }
}

/// Guard the node, reading the proxy's fitness from `source`, until
/// `terminate` resolves and the proxy then stops answering. `Err` when the
/// first ruleset cannot be written.
pub async fn run(
    cfg: &GuardConfig,
    source: &impl Source,
    writer: &impl Writer,
    metrics: &GuardMetrics,
    pulse: &Pulse,
    terminate: impl Future<Output = ()>,
) -> Result<(), String> {
    let first = source.read().await;
    let now = Instant::now();
    let (mut switch, write, cause) = Switch::start(cfg.timing, now, &first);
    writer
        .write(script(cfg, write), ONE_TIME_WRITE_BOUND)
        .await
        .map_err(|e| format!("the guard's first ruleset was not applied: {e}"))?;
    if write == Write::Refresh {
        switch.refreshed(now);
    }
    if let Some(cause) = &cause {
        metrics.withdrawal(cause);
    }
    metrics.set_withdrawn(switch.withdrawn());
    tracing::info!(
        peers = %cfg.peers.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        fitness = %cfg.fitness,
        poll_ms = cfg.poll.as_millis(),
        dead_ms = cfg.timing.dead.as_millis(),
        hold_ms = cfg.timing.hold.as_millis(),
        withdrawn = switch.withdrawn(),
        reading = ?first,
        "route guard started"
    );

    tokio::pin!(terminate);
    let mut terminating = false;
    #[expect(clippy::disallowed_methods, reason = "the poll is at least 10 ms")]
    let mut tick = tokio::time::interval_at(Instant::now() + cfg.poll, cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = &mut terminate, if !terminating => {
                terminating = true;
                tracing::info!("terminating: guarding until the proxy stops answering");
                continue;
            }
        }
        pulse.beat();
        let reading = source.read().await;
        if let Reading::NoAnswer(_) = reading {
            metrics.no_answer();
            if terminating {
                tracing::info!(
                    reading = ?reading,
                    dead_ms = cfg.timing.dead.as_millis(),
                    "the proxy is gone: exiting, the node's last refresh runs out"
                );
                return Ok(());
            }
        }
        let now = Instant::now();
        let step = switch.on_reading(now, &reading);
        if let Some(cause) = &step.withdrawn {
            withdrawn(metrics, cause, &reading);
        }
        if let Some(write) = step.write {
            match writer.write(script(cfg, write), cfg.timing.dead / 2).await {
                Ok(()) if write == Write::Refresh && switch.refreshed(now) => {
                    metrics.restore();
                    tracing::info!(
                        hold_ms = cfg.timing.hold.as_millis(),
                        "node let out: BFD and BGP to the routers pass again"
                    );
                }
                Ok(()) => {}
                Err(e) => {
                    metrics.write_failure();
                    tracing::warn!(error = %e, ?write, "ruleset not applied");
                }
            }
        }
        metrics.set_withdrawn(switch.withdrawn());
    }
}

fn script(cfg: &GuardConfig, write: Write) -> String {
    match write {
        Write::Refresh => ruleset(&cfg.peers, Some(cfg.timing.dead)),
        Write::Withdraw => ruleset(&cfg.peers, None),
    }
}

/// The node withdrawn for `cause`, `reading` being the reading that saw it.
fn withdrawn(metrics: &GuardMetrics, cause: &Cause, reading: &Reading) {
    metrics.withdrawal(cause);
    let reason = match cause {
        Cause::Unfit(reason) => reason.as_str(),
        Cause::Expired | Cause::Start => "",
    };
    tracing::warn!(
        cause = cause.label(),
        reason,
        reading = ?reading,
        "node withdrawn: BFD and BGP to the routers dropped"
    );
}

#[cfg(test)]
mod tests;
