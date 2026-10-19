//! The worker shell's own health: the poisoned locks it recovered and the
//! supervised tasks it restarted.

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
