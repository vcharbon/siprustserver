//! Poison-tolerant locking (ADR-0051 §7): a panic under one of this crate's
//! locks never stops the node. Every critical section leaves its state whole
//! between statements, so a poisoned lock is taken as it stands, its poison
//! cleared, and the recovery logged and counted once per panic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

static RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// Lock `m`, taking a poisoned guard as it stands. `what` names the lock in
/// the warning a recovery logs.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    #[expect(clippy::disallowed_methods, reason = "the crate's one lock call, poison recovered")]
    let lock = m.lock();
    lock.unwrap_or_else(|poisoned| {
        let guard = poisoned.into_inner();
        m.clear_poison();
        let total = RECOVERIES.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        tracing::warn!(lock = what, total, "a panic poisoned a lock; recovered as it stands");
        guard
    })
}

/// This crate's locks recovered from a panic in this process.
pub fn poisoned_lock_recoveries() -> u64 {
    RECOVERIES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{locked, poisoned_lock_recoveries};

    #[test]
    fn a_poisoned_lock_is_recovered_once_and_counted() {
        let m = Arc::new(Mutex::new(1));
        let poisoner = m.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("poison the lock");
        })
        .join();
        let before = poisoned_lock_recoveries();
        assert_eq!(*locked(&m, "test"), 1);
        assert!(!m.is_poisoned(), "the poison is cleared");
        assert_eq!(*locked(&m, "test"), 1);
        assert_eq!(poisoned_lock_recoveries() - before, 1, "one recovery per panic");
    }
}
