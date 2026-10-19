//! The crate's named lock (ADR-0051 §7): every [`crate::OpenRows`] takes its
//! rows under `open_rows`, and a poisoned one is recovered and counted
//! (`metric_values_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

const OPEN_ROWS: &str = "open_rows";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 1] = [OPEN_ROWS];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock the rows `m` of an open family.
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    RECOVERIES.mutex(m, OPEN_ROWS)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "metric_values_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "metric row locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`], appended by a runner's `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
