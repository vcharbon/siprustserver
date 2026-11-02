//! What the replication hold publishes (ADR-0055 §3): its outbox depth, its
//! releases by reason, and the flows this node serves by state, so a backup
//! that stalls shows as releases leaving unheld rather than as a quiet cap
//! counter.

use std::sync::atomic::{AtomicU64, Ordering};

use super::{catalogue, cells, B2buaMetrics};
use crate::repl::FlowLiveness;
use crate::turn_probe::Released;

/// The hold's cells.
#[derive(Debug, Default)]
pub(super) struct ReplHold {
    /// Outbox entries queued across every call (a gauge).
    depth: AtomicU64,
    /// Outbox releases under the `applied` level, indexed like [`Released::ALL`].
    released: [AtomicU64; Released::ALL.len()],
    /// The flows served now, indexed like [`FlowLiveness::ALL`] (a gauge).
    flows: [AtomicU64; FlowLiveness::ALL.len()],
}

impl ReplHold {
    /// Write the depth, the releases, then the flows.
    pub(super) fn render(&self, s: &mut String) {
        catalogue::capacity::REPL_OUTBOX_DEPTH.render_value(s, self.depth.load(Ordering::Relaxed));
        catalogue::capacity::REPL_OUTBOX_RELEASED.render(s, |series| {
            cells::get_at(&self.released, series.index(&catalogue::capacity::RELEASE_REASON))
        });
        catalogue::capacity::REPL_SERVING_FLOWS.render(s, |series| {
            cells::get_at(&self.flows, series.index(&catalogue::capacity::FLOW_STATE))
        });
    }
}

impl B2buaMetrics {
    /// Publish the outbox entries queued across every call (ADR-0055 §3).
    pub fn set_repl_outbox_depth(&self, depth: u64) {
        self.inner.repl_hold.depth.store(depth, Ordering::Relaxed);
    }

    /// Count one outbox entry released for `reason` under the `applied` level.
    pub fn count_outbox_released(&self, reason: Released) {
        let at = Released::ALL.iter().position(|r| *r == reason).unwrap_or(usize::MAX);
        cells::add_at(&self.inner.repl_hold.released, at, 1);
    }

    /// Outbox entries released for `reason` (test/observability).
    pub fn outbox_released_total(&self, reason: Released) -> u64 {
        let at = Released::ALL.iter().position(|r| *r == reason).unwrap_or(usize::MAX);
        cells::get_at(&self.inner.repl_hold.released, at)
    }

    /// Publish how many served flows stand in each state, indexed like
    /// [`FlowLiveness::ALL`].
    pub fn set_repl_serving_flows(&self, counts: [u64; FlowLiveness::ALL.len()]) {
        for (cell, n) in self.inner.repl_hold.flows.iter().zip(counts) {
            cell.store(n, Ordering::Relaxed);
        }
    }

    /// The served flows last published in `state` (test/observability).
    pub fn repl_serving_flows(&self, state: FlowLiveness) -> u64 {
        let at = FlowLiveness::ALL.iter().position(|s| *s == state).unwrap_or(usize::MAX);
        cells::get_at(&self.inner.repl_hold.flows, at)
    }
}
