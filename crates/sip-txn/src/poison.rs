//! The transaction layer's named locks (ADR-0051 §7): every std lock of the
//! crate is taken through [`locked`] under one of these names, and a
//! poisoned one is recovered and counted under it
//! (`b2bua_txn_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

pub(crate) const INVITE_REFUSALS_MEMO: &str = "invite_refusals_memo";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 1] = [INVITE_REFUSALS_MEMO];

/// The recoveries of the layer's locks in this process.
pub static LOCK_RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    LOCK_RECOVERIES.mutex(m, name)
}
