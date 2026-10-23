//! The kit's named locks (ADR-0051 §7): every std lock of the worker kit is
//! taken through `locked` under one of these names, and a poisoned one is
//! recovered and counted under it
//! (`b2bua_runner_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

pub(crate) const CDR_SOCKET_KILL: &str = "cdr_socket_kill";
pub(crate) const REPL_TARGETS: &str = "repl_targets";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 2] = [CDR_SOCKET_KILL, REPL_TARGETS];

/// The recoveries of the kit's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "b2bua_runner_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "worker kit locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families [`prometheus_text`] renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`].
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
