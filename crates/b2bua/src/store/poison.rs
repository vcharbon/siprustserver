//! The store's named locks (ADR-0051 §7). Every critical section over a store
//! map writes only values computed before it and writes the call map last on
//! an insert, first on a removal, so a panic under the lock leaves at most
//! derived entries the sweep reconciles: a poisoned lock is taken as it
//! stands, its poison cleared, and the recovery logged and counted under its
//! name (`b2bua_store_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard};

use lock_poison::Recoveries;

pub(crate) const CALL_STORE: &str = "call_store";
pub(crate) const REPLICA_BODIES: &str = "replica_bodies";
pub(crate) const REPLICA_METADATA: &str = "replica_metadata";
pub(crate) const REPLICA_CHANGELOG: &str = "replica_changelog";
pub(crate) const RESURRECTION_TOMBSTONES: &str = "resurrection_tombstones";
pub(crate) const DISPLACED_BODIES: &str = "displaced_bodies";
pub(crate) const REPLICA_REFUSALS: &str = "replica_refusals";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 7] = [
    CALL_STORE,
    REPLICA_BODIES,
    REPLICA_METADATA,
    REPLICA_CHANGELOG,
    RESURRECTION_TOMBSTONES,
    DISPLACED_BODIES,
    REPLICA_REFUSALS,
];

/// The recoveries of the store's locks in this process.
pub static LOCK_RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the store lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    LOCK_RECOVERIES.mutex(m, name)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{locked, LOCK_RECOVERIES, REPLICA_REFUSALS};

    #[test]
    fn a_poisoned_lock_is_recovered_once_and_counted_under_its_name() {
        let m = Arc::new(Mutex::new(1));
        let poisoner = m.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("poison the lock");
        })
        .join();
        let before = LOCK_RECOVERIES.count(REPLICA_REFUSALS);
        assert_eq!(*locked(&m, REPLICA_REFUSALS), 1);
        assert!(!m.is_poisoned(), "the poison is cleared");
        assert_eq!(*locked(&m, REPLICA_REFUSALS), 1);
        assert_eq!(LOCK_RECOVERIES.count(REPLICA_REFUSALS) - before, 1, "one recovery per panic");
    }
}
