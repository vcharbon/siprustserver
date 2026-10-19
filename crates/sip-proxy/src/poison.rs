//! Poison-tolerant locking (ADR-0051 §7): a panic under one of the proxy's
//! locks never stops it. Every critical section leaves its state whole
//! between statements, so a poisoned lock is taken as it stands, its poison
//! cleared, and the recovery logged and counted once per panic
//! (`sip_proxy_poisoned_lock_recoveries_total`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{
    LockResult, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

static RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// The guard of `lock`, its poison cleared by `clear` and the recovery
/// counted when a panic poisoned it. `what` names the lock in the warning.
fn recovered<G>(lock: LockResult<G>, clear: impl FnOnce(), what: &'static str) -> G {
    lock.unwrap_or_else(|poisoned: PoisonError<G>| {
        let guard = poisoned.into_inner();
        clear();
        let total = RECOVERIES.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        tracing::warn!(lock = what, total, "a panic poisoned a proxy lock; recovered as it stands");
        guard
    })
}

/// Lock `m`, taking a poisoned guard as it stands.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    #[expect(clippy::disallowed_methods, reason = "the proxy's one Mutex lock, poison recovered")]
    let lock = m.lock();
    recovered(lock, || m.clear_poison(), what)
}

/// Read-lock `rw`, taking a poisoned guard as it stands.
pub(crate) fn read<'a, T>(rw: &'a RwLock<T>, what: &'static str) -> RwLockReadGuard<'a, T> {
    #[expect(clippy::disallowed_methods, reason = "the proxy's one read lock, poison recovered")]
    let lock = rw.read();
    recovered(lock, || rw.clear_poison(), what)
}

/// Write-lock `rw`, taking a poisoned guard as it stands.
pub(crate) fn write<'a, T>(rw: &'a RwLock<T>, what: &'static str) -> RwLockWriteGuard<'a, T> {
    #[expect(clippy::disallowed_methods, reason = "the proxy's one write lock, poison recovered")]
    let lock = rw.write();
    recovered(lock, || rw.clear_poison(), what)
}

/// The proxy's locks recovered from a panic in this process.
pub fn poisoned_lock_recoveries() -> u64 {
    RECOVERIES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, RwLock};

    use super::{locked, poisoned_lock_recoveries, read, write};

    #[test]
    fn a_poisoned_lock_is_recovered_once_and_counted() {
        let m = Arc::new(Mutex::new(1));
        let held = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison the mutex");
        })
        .join();
        let rw = Arc::new(RwLock::new(2));
        let held = rw.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.write();
            panic!("poison the rwlock");
        })
        .join();

        let before = poisoned_lock_recoveries();
        assert_eq!(*locked(&m, "test"), 1);
        assert!(!m.is_poisoned(), "the poison is cleared");
        assert_eq!(*read(&rw, "test"), 2);
        assert!(!rw.is_poisoned(), "the poison is cleared");
        assert_eq!(*write(&rw, "test"), 2);
        assert_eq!(*locked(&m, "test"), 1);
        assert_eq!(poisoned_lock_recoveries() - before, 2, "one recovery per panic");
    }
}
