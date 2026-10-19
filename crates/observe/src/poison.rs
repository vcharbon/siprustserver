//! The crate's named locks (ADR-0051 §7): every std lock of `observe` outside
//! the panic hook is taken through [`locked`] under one of these names, and a
//! poisoned one is recovered and counted under it
//! (`observe_poisoned_lock_recoveries_total{lock}`). The panic hook's own lock
//! recovers without counting: it runs inside a panic.

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

pub(crate) const ACTIVATION_BUCKET: &str = "activation_bucket";
pub(crate) const WAVE_KEYS: &str = "wave_keys";
pub(crate) const WAVE_SLOT: &str = "wave_slot";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 3] = [ACTIVATION_BUCKET, WAVE_KEYS, WAVE_SLOT];

/// The recoveries of the crate's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}
