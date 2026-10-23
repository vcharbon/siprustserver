//! The worker shell's own health: the poisoned locks it recovered, the
//! supervised tasks it restarted and the store entries its sweep corrected.
//! A run that injects no fault ends with none of them.

use super::catalogue::shell::{
    POISONED_LOCK_RECOVERIES, STORE_POISONED_LOCK_RECOVERIES, TASK_RESTARTS,
};

/// Append the three families, one series per named lock or task.
pub(super) fn render(s: &mut String) {
    STORE_POISONED_LOCK_RECOVERIES
        .render(s, |series| crate::store::LOCK_RECOVERIES.count_at(series.at(0)));
    POISONED_LOCK_RECOVERIES.render(s, |series| crate::poison::RECOVERIES.count_at(series.at(0)));
    TASK_RESTARTS.render(s, |series| crate::supervise::RESTARTS.count_at(series.at(0)));
}

/// The poisoned locks this process recovered (the store's, the worker's and
/// the metric rows') and the supervised tasks it restarted, all counted.
pub fn shell_faults() -> u64 {
    let store = crate::store::LOCKS.len();
    let worker = crate::poison::LOCKS.len();
    let rows = metric_values::poison::LOCKS.len();
    let tasks = crate::supervise::TASKS.len();
    (0..store).map(|at| crate::store::LOCK_RECOVERIES.count_at(at)).sum::<u64>()
        + (0..worker).map(|at| crate::poison::RECOVERIES.count_at(at)).sum::<u64>()
        + (0..rows).map(|at| metric_values::poison::RECOVERIES.count_at(at)).sum::<u64>()
        + (0..tasks).map(|at| crate::supervise::RESTARTS.count_at(at)).sum::<u64>()
}

impl super::B2buaMetrics {
    /// The store entries the reaper sweep corrected, every kind.
    pub fn store_reconciled_total(&self) -> u64 {
        self.store_reconciled_orphan_stamps_total()
            + self.store_reconciled_missing_stamps_total()
            + self.store_reconciled_orphan_index_total()
            + self.store_reconciled_missing_index_total()
            + self.store_reconciled_orphan_takeover_total()
    }
}
