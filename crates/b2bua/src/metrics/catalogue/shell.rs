//! The worker shell's own health families: the poisoned locks it recovered
//! and the supervised tasks it restarted, one series per named lock or task.

use metric_catalogue::{Dim, Family, Labels};

pub const STORE_POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "b2bua_store_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &crate::store::LOCKS)]),
    "store locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "b2bua_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &crate::poison::LOCKS)]),
    "worker locks outside the store a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

pub const TASK_RESTARTS: Family = Family::counter(
    "b2bua_task_restarts_total",
    Labels::Product(&[Dim::new("task", &crate::supervise::TASKS)]),
    "runs of a long-lived worker task that panicked and were made again (process-wide), by task — expected 0",
);
