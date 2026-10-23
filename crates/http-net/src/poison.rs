//! The crate's named locks (ADR-0051 §7): every std lock of `http-net` is
//! taken through `locked` under one of these names, and a poisoned one is
//! recovered and counted under it
//! (`http_request_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

pub(crate) const BACKEND_BREAKER: &str = "backend_breaker";
pub(crate) const BACKEND_PROBE: &str = "backend_probe";
pub(crate) const BACKEND_REPORT: &str = "backend_report";
pub(crate) const BACKEND_ROTATION: &str = "backend_rotation";
pub(crate) const RESOLVED_NAME: &str = "resolved_name";
pub(crate) const MEMORY_RESOLVER: &str = "memory_resolver";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 6] = [
    BACKEND_BREAKER,
    BACKEND_PROBE,
    BACKEND_REPORT,
    BACKEND_ROTATION,
    RESOLVED_NAME,
    MEMORY_RESOLVER,
];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "http_request_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "HTTP client locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`], appended by a runner's `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
