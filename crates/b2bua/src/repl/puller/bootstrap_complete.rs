//! A puller's bootstrap completing: the sticky mark the supervisor reads,
//! and the bulk reclaim the Reclaim flow fires on it (ADR-0011 X5, ADR-0014
//! §4).

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

    /// Signal the router to **bulk-reclaim** every `pri:{self}` body — materialise
    /// each into the live serving map + re-arm its timers, with the keepalive
    /// backlog smoothed (ADR-0014 §4). Fired by the **Reclaim** flow on bootstrap
    /// completion (first `Noop` or best-effort hard-timer). Idempotent
    /// (`materialize_if_absent`), so a partial pass plus a later one self-heal.
    pub(super) fn signal_reclaim_all(&self) {
        if let Some(sink) = &self.repl_tx {
            sink.send(ReplCommand::ReclaimAll);
        }
    }
}
