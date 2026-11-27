//! A puller's bootstrap completing: the sticky mark the supervisor reads,
//! and the bulk reclaim the Reclaim flow fires on it (ADR-0011 X5, ADR-0014
//! §4).
//!
//! Completion comes from the first catch-up `Noop` (every body of the scan
//! applied) or, best-effort, from the bootstrap hard timer (bodies may be
//! missing). It releases what must not wait on a peer that cannot be reached:
//! the readiness of a flow that never connected (the supervisor's
//! `FlowState::ready` and `PeerEntry::ready`), `all_bootstrapped`, and the
//! Reclaim flow's bulk reclaim of what the scan imported. It never reads as the calls being held: a drain's
//! caught-up read and the hold read the `Position` claim, which only a
//! catch-up `Noop` or a post-bootstrap `Data` answers, and a reached flow's
//! readiness reads `current`.

use std::sync::atomic::Ordering;

use super::Puller;
use crate::router::ReplCommand;

impl Puller {
    /// Mark this puller **bootstrap-complete** (sticky). Called on the first
    /// catch-up `Noop` and on the hard-timer firing (best-effort). Lines the
    /// per-peer completion once — `applied` bodies imported, `reason` telling a
    /// real catch-up from a best-effort timer expiry.
    pub(super) fn mark_bootstrap_complete(&self, applied: u64, reason: &'static str) {
        if !self.status_tx.borrow().bootstrap_complete {
            let started =
                crate::poison::locked(&self.bootstrap_started_at, crate::poison::PULLER_BOOTSTRAP)
                    .take();
            let duration_ms = started.map_or(0, |at| sip_clock::wall::millis_u64(at.elapsed()));
            tracing::info!(
                node = observe::node(),
                peer = %self.peer.ordinal,
                flow = self.flow_label(),
                applied,
                duration_ms,
                reason,
                "peer bootstrap complete"
            );
        }
        self.status_tx.send_modify(|s| s.bootstrap_complete = true);
    }

    /// The bootstrap hard timer ran out before the first catch-up `Noop`, on
    /// either flow: counted on `b2bua_repl_bootstrap_stalled_total`, then the
    /// bootstrap completes best-effort. On a flow that reached its peer the
    /// scan stopped short, so a warning names the bodies applied: `applied`
    /// when a stream is open, else those of the last cold stream.
    pub(super) fn bootstrap_timer_fired(&self, applied: Option<u64>, reason: &'static str) {
        self.metrics.bump_repl_bootstrap_stalled();
        let applied = applied.unwrap_or_else(|| self.scan_applied.load(Ordering::Relaxed));
        if self.status_tx.borrow().ever_connected {
            tracing::warn!(
                node = observe::node(),
                peer = %self.peer.ordinal,
                flow = self.flow_label(),
                applied,
                reason,
                "bootstrap hard timer ran out on a reached peer: complete best-effort, the bodies \
                 past those applied are not held until a scan reaches its catch-up Noop"
            );
        }
        self.mark_bootstrap_complete(applied, reason);
    }

    /// A cold stream ended before its first catch-up `Noop` after applying
    /// `applied` bodies: counted on `b2bua_repl_bootstrap_dropped_total`, and
    /// kept for a hard timer that runs out before the next stream opens. The
    /// next pull scans again from the first key.
    pub(super) fn bootstrap_dropped(&self, applied: u64) {
        self.metrics.record_repl_bootstrap_dropped(self.flow_label());
        self.scan_applied.store(applied, Ordering::Relaxed);
        self.note_state(
            "bootstrap_dropped",
            &format!(
                "the scan ended after {applied} bodies, before its first catch-up Noop: the next \
                 pull scans again from the first key"
            ),
        );
    }

    /// Signal the router to **bulk-reclaim** every `pri:{self}` body — materialise
    /// each into the live serving map, parked, then re-arm its timers with the
    /// keepalive backlog smoothed (ADR-0014 §4). Fired by the **Reclaim** flow on
    /// bootstrap completion (first `Noop` or best-effort hard-timer); the router
    /// parks at once and arms while the node's own endpoint reads ready
    /// (ADR-0055 §8). Idempotent
    /// (`materialize_if_absent`), so a partial pass plus a later one self-heal.
    pub(super) fn signal_reclaim_all(&self) {
        if let Some(sink) = &self.repl_tx {
            sink.send(ReplCommand::ReclaimAll);
        }
    }
}
