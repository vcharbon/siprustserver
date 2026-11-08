//! The crate's named lock (ADR-0051 §7): every [`crate::queue::PacketQueue`]
//! takes its state under `packet_queue`, and a poisoned one is recovered and
//! counted (`sip_net_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;
use metric_catalogue::{Dim, Family, Labels};

const PACKET_QUEUE: &str = "packet_queue";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 1] = [PACKET_QUEUE];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock a queue's state `m`.
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    RECOVERIES.mutex(m, PACKET_QUEUE)
}

/// The recoveries, by lock.
pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "sip_net_poisoned_lock_recoveries_total",
    Labels::Product(&[Dim::new("lock", &LOCKS)]),
    "UDP endpoint locks a panic poisoned, taken as they stood and cleared (process-wide), by lock — expected 0",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[POISONED_LOCK_RECOVERIES];

/// Prometheus exposition of [`FAMILIES`], appended by a runner's `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    POISONED_LOCK_RECOVERIES.render(&mut s, |series| RECOVERIES.count_at(series.at(0)));
    s
}
