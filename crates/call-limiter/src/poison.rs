//! The crate's named lock (ADR-0051 §7): the [`crate::CallStore`] takes its
//! state under `store`, and a poisoned one is recovered and counted
//! (`limiter_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

const STORE: &str = "store";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 1] = [STORE];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock the store's state `m`.
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    RECOVERIES.mutex(m, STORE)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "limiter_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "limiter locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`], appended to the limiter's
/// `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
