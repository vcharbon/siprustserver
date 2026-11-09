//! The watchdog's named lock (ADR-0051 §7), taken through `locked`; a
//! poisoned one is recovered and counted under its name.

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

pub(crate) const SLOTS: &str = "poll_watchdog_slots";

/// The recoveries of the watchdog's lock in this process.
pub static RECOVERIES: Recoveries<1> = Recoveries::new([SLOTS]);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}
