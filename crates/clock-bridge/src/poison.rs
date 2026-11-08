//! The crate's named lock (ADR-0051 §7): a [`crate::FaultLog`] takes its
//! faults under `fault_log`, and a poisoned one is recovered, logged and
//! counted under it.

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

const FAULT_LOG: &str = "fault_log";

/// Every lock name, in counter order.
pub const LOCKS: [&str; 1] = [FAULT_LOG];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock a fault log's faults `m`.
pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    RECOVERIES.mutex(m, FAULT_LOG)
}
