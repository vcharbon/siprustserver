//! How a worker stands down: its own withdrawal from routing (ADR-0031 D6),
//! the `Draining` latch, and the D2 drain over its inputs, the end of the
//! calls no peer holds at its grace included (ADR-0055 §3).

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use repl_net::frame::Partition;

use super::B2buaCore;
use crate::drain::GraceEnd;
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

    /// The probes the drain reads and the end it runs at its grace, OWNED —
    /// so a caller can run the drain without borrowing this core (the harness
    /// drives it across a paused timeline).
    pub fn drain_probe(&self) -> crate::drain::DrainInputs {
        let ctx = self.ctx.clone();
        let changelog = self.repl_store.as_ref().map(|r| r.changelog().clone());
        let end_unheld = Arc::new({
            let (ctx, changelog, node) = (self.ctx.clone(), changelog.clone(), self.node.clone());
            move || node.enter(|| end_unheld(&ctx, changelog.as_ref()))
        });
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
            outboxes_waiting: Arc::new({
                let outboxes = self.ctx.outboxes.clone();
                move || outboxes.depth()
            }),
            end_unheld,
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

/// End each live call no live peer holds, at the drain's grace: the call has
/// no backup, or its holder's flow is not live (away, or still bootstrapping).
/// Each is sent the release turn of ADR-0050 §10, on its own queue: an
/// established call ends where it stands, its record closed and nothing sent;
/// a call in setup is aborted, `503` to its caller and a CANCEL to a ringing
/// callee. A call a live peer holds, caught up or not, is left to it. An
/// ending that does not leave in time (a withdrawn worker never sees the
/// answer to its CANCEL: the proxy hands it to the call's other owner) is
/// discharged off the wire, as the reaper discharges a wedged call.
fn end_unheld(ctx: &Arc<RouterCtx>, changelog: Option<&crate::repl::Changelog>) -> GraceEnd {
    use crate::rules::operator_release::{AT_DRAIN_GRACE, TOPIC};
    let me = &ctx.config.self_ordinal;
    let unheld = ctx.state.unheld_active_calls(|t| {
        let (holder, partition) =
            if t.pri == *me { (&t.bak, Partition::Bak) } else { (&t.pri, Partition::Pri) };
        !holder.is_empty() && changelog.is_some_and(|cl| cl.flow_live(holder, partition))
    });
    for (call_ref, incarnation) in &unheld {
        let _ = ctx.reentry_tx.send(CallEvent::InternalEvent {
            call_ref: call_ref.clone(),
            topic: TOPIC.to_string(),
            outcome: AT_DRAIN_GRACE.to_string(),
            payload: serde_json::Value::Null,
            body: Vec::new(),
            incarnation: Some(incarnation.clone()),
        });
    }
    if !unheld.is_empty() {
        tracing::info!(
            node = observe::node(),
            ended = unheld.len(),
            "drain grace: ending the calls no peer holds"
        );
    }
    let ended = unheld.len();
    let refs: Arc<Vec<String>> =
        Arc::new(unheld.into_iter().map(|(call_ref, _)| call_ref).collect());
    let left = {
        let (ctx, refs) = (ctx.clone(), refs.clone());
        Arc::new(move || refs.iter().filter(|r| ctx.state.contains(r)).count())
    };
    let discharge_left = {
        let ctx = ctx.clone();
        Arc::new(move || {
            for call_ref in refs.iter().filter(|r| ctx.state.contains(r)) {
                let _ = ctx.reentry_tx.send(CallEvent::InternalEvent {
                    call_ref: call_ref.clone(),
                    topic: crate::reaper::REAPER_TOPIC.to_string(),
                    outcome: crate::reaper::OUTCOME_DISCHARGE.to_string(),
                    payload: serde_json::json!({}),
                    body: Vec::new(),
                    incarnation: None,
                });
            }
        })
    };
    GraceEnd { ended, left, discharge_left }
}
