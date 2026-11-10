//! The watchdog's named lock (ADR-0051 §7), taken through `locked`; a
//! poisoned one is recovered and counted under its name
//! (`poll_watchdog_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

pub(crate) const SLOTS: &str = "poll_watchdog_slots";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 1] = [SLOTS];

/// The recoveries of the watchdog's lock in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "poll_watchdog_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "poll watchdog locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`], appended by a runner's `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
