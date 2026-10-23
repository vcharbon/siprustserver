//! How a worker stands down: its own withdrawal from routing (ADR-0031 D6),
//! the `Draining` latch, and the D2 drain over its inputs.

use std::sync::Arc;

use super::B2buaCore;
use crate::router::RouterCtx;

impl B2buaCore {
    /// Whether this worker has observed its own endpoint withdrawn from routing
    /// (ADR-0031 D6): the proxy routes nothing new here. `false` without
    /// replication or on a membership that does not show the worker itself.
    pub fn is_withdrawn(&self) -> bool {
        self.supervisor.as_ref().is_some_and(|s| s.is_withdrawn())
    }

    /// Resolves once this worker has observed its own endpoint withdrawn
    /// from routing (ADR-0031 D6); never without replication or on a
    /// membership that does not show the worker itself. The host drains on it
    /// as on SIGTERM, and exits (ADR-0031 D5).
    pub async fn withdrawn(&self) {
        if let Some(supervisor) = &self.supervisor {
            let mut own = supervisor.self_endpoint();
            if own.wait_for(|own| own.is_withdrawn()).await.is_ok() {
                return;
            }
        }
        std::future::pending().await
    }

    /// Latch this worker into the `Draining` readiness state (SIGTERM → drain).
    /// OPTIONS then self-reports `503 draining` so the front proxy steers new
    /// calls away while in-flight calls finish. Terminal — never un-drains.
    ///
    /// SIGTERM wiring: the **runner** installs the `tokio::signal` SIGTERM hook
    /// that calls this; the library exposes the method rather than installing
    /// the hook so tests/embedders control the signal surface.
    pub fn begin_draining(&self) {
        self.readiness.set_draining();
    }

    /// Whether every live call this worker serves is held by a peer that has
    /// applied this worker's changelog head (ADR-0031 D2). The other copy's
    /// holder is `topology.bak` for a call this worker is primary for (the peer
    /// pulls the Backup flow) and `topology.pri` for a takeover copy it serves
    /// as backup (the peer pulls the Reclaim flow). An empty holder ordinal, a
    /// disconnected flow, a flow behind the head, or replication being off ⇒
    /// `false`: nothing proves the call survives this worker's exit.
    pub fn backups_caught_up(&self) -> bool {
        let Some(repl) = &self.repl_store else {
            return false;
        };
        backups_caught_up_in(&self.ctx, repl.changelog())
    }

    /// The three probes the drain reads, OWNED — so a caller can run the drain
    /// without borrowing this core (the harness drives it across a paused
    /// timeline). Read-only.
    pub fn drain_probe(&self) -> crate::drain::DrainInputs {
        let ctx = self.ctx.clone();
        let changelog = self.repl_store.as_ref().map(|r| r.changelog().clone());
        let supervisor = self.supervisor.clone();
        let limiter = self.ctx.limiter.clone();
        crate::drain::DrainInputs {
            active: self.active_calls_probe(),
            backups_caught_up: Arc::new(move || match &changelog {
                Some(cl) => backups_caught_up_in(&ctx, cl),
                None => false,
            }),
            withdrawn: Arc::new(move || supervisor.as_ref().is_some_and(|s| s.is_withdrawn())),
            flush_releases: Arc::new({
                let limiter = limiter.clone();
                move |within| {
                    let limiter = limiter.clone();
                    Box::pin(async move { limiter.flush(within).await })
                }
            }),
            releases_waiting: Arc::new(move || limiter.unsent()),
        }
    }

    /// Graceful shutdown: latch `Draining` (so the proxy steers new calls away
    /// via the OPTIONS / `/ready` self-report) and then wait for the first of:
    /// the live call map clearing, a withdrawn worker's backups holding every
    /// live call past the floor (ADR-0031 D2), or the grace; the limiter
    /// release queue is flushed before the exit, a clean exit re-verified
    /// after it ([`drain_until_quiescent`](crate::drain::drain_until_quiescent)).
    /// Returns the named exit, the residual active-call count, how long it
    /// waited and what the flush did; the cut is never silent. `Draining` is
    /// the single home for the drain state — there is no second flag to keep
    /// in sync.
    pub async fn drain(&self, bounds: crate::drain::DrainBounds) -> crate::drain::DrainOutcome {
        self.begin_draining();
        let outcome = crate::drain::drain_until_quiescent(self.drain_probe(), bounds).await;
        self.metrics.record_drain_exit(&outcome);
        let flush = outcome.release_flush;
        tracing::info!(
            reason = outcome.exit.label(),
            residual = outcome.residual,
            elapsed_ms = sip_clock::wall::millis_u64(outcome.elapsed),
            withdrawn = self.is_withdrawn(),
            release_flush = flush.outcome().label(),
            releases_queued = flush.queued,
            releases_given_up = flush.given_up,
            release_flush_ms = sip_clock::wall::millis_u64(flush.elapsed),
            "drain returned"
        );
        outcome
    }
}

/// The [`B2buaCore::backups_caught_up`] predicate over a context + changelog, so
/// both the borrowing accessor and the owned [`B2buaCore::drain_probe`] closure
/// read one implementation. The drain polls it every tick: it reads the distinct
/// topologies of the resident calls under one lock, no call cloned, and asks
/// each holder's flow once.
fn backups_caught_up_in(ctx: &RouterCtx, changelog: &crate::repl::Changelog) -> bool {
    use repl_net::frame::Partition;
    let me = &ctx.config.self_ordinal;
    // A call naming no topology, or with no backup, names no holder: nothing
    // can be holding it.
    let Some(topologies) = ctx.state.live_topologies() else {
        return false;
    };
    topologies.iter().all(|(pri, bak)| {
        let (holder, partition) =
            if pri == me { (bak, Partition::Bak) } else { (pri, Partition::Pri) };
        !holder.is_empty() && changelog.flow_caught_up(holder, partition)
    })
}
