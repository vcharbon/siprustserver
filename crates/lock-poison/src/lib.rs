//! Poison-tolerant locking (ADR-0051 §7): a panic under a std lock never
//! stops the node. A crate names its locks once, in a [`Recoveries`] table,
//! and takes every lock through it. Every critical section leaves its state
//! whole between statements, so a poisoned lock is taken as it stands, its
//! poison cleared, and the recovery logged and counted against the lock's
//! name, once per panic.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A crate's named locks and the recoveries of each, in the order of
/// [`names`](Self::names): the values of a `lock` label. A name a lock is
/// taken under is one of them.
pub struct Recoveries<const N: usize> {
    names: [&'static str; N],
    counts: [AtomicU64; N],
}

impl<const N: usize> Recoveries<N> {
    /// The table of the locks `names`, none recovered.
    pub const fn new(names: [&'static str; N]) -> Self {
        Self { names, counts: [const { AtomicU64::new(0) }; N] }
    }

    /// The lock names, in counter order.
    pub const fn names(&self) -> &[&'static str; N] {
        &self.names
    }

    /// Recoveries of the lock named `name`; a name outside the table has none.
    pub fn count(&self, name: &str) -> u64 {
        self.slot(name).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Recoveries of the lock at `position` in [`names`](Self::names).
    pub fn count_at(&self, position: usize) -> u64 {
        self.counts.get(position).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    fn slot(&self, name: &str) -> Option<&AtomicU64> {
        self.names.iter().position(|n| *n == name).and_then(|i| self.counts.get(i))
    }

    /// Count and log one recovery of `name`. Called while the recovering
    /// thread holds the lock exclusively, so each panic is counted once.
    fn recovered(&self, name: &'static str) {
        let total = match self.slot(name) {
            Some(count) => count.fetch_add(1, Ordering::Relaxed).saturating_add(1),
            None => {
                debug_assert!(false, "{name} is no lock of this table");
                0
            }
        };
        tracing::warn!(lock = name, total, "a panic poisoned a lock; recovered as it stands");
    }

    /// Lock `m`, taking a poisoned guard as it stands.
    pub fn mutex<'a, T>(&self, m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
        #[expect(clippy::disallowed_methods, reason = "the helper's Mutex lock, poison recovered")]
        let lock = m.lock();
        lock.unwrap_or_else(|poisoned| {
            m.clear_poison();
            self.recovered(name);
            poisoned.into_inner()
        })
    }

    /// Write-lock `rw`, taking a poisoned guard as it stands.
    pub fn write<'a, T>(&self, rw: &'a RwLock<T>, name: &'static str) -> RwLockWriteGuard<'a, T> {
        #[expect(clippy::disallowed_methods, reason = "the helper's write lock, poison recovered")]
        let lock = rw.write();
        lock.unwrap_or_else(|poisoned| {
            rw.clear_poison();
            self.recovered(name);
            poisoned.into_inner()
        })
    }

    /// Read-lock `rw`, taking a poisoned guard as it stands. Readers share
    /// the lock, so the poison is cleared and counted under the write lock,
    /// by the first reader that takes it.
    pub fn read<'a, T>(&self, rw: &'a RwLock<T>, name: &'static str) -> RwLockReadGuard<'a, T> {
        #[expect(clippy::disallowed_methods, reason = "the helper's read lock, poison recovered")]
        let lock = rw.read();
        match lock {
            Ok(guard) => guard,
            Err(poisoned) => {
                drop(poisoned);
                drop(self.write(rw, name));
                #[expect(clippy::disallowed_methods, reason = "the helper's read lock, re-taken")]
                let again = rw.read();
                // Poisoned again only by a later panic, which the next caller counts.
                again.unwrap_or_else(PoisonError::into_inner)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, RwLock};

    use super::Recoveries;

    static TABLE: Recoveries<2> = Recoveries::new(["one", "two"]);

    fn poison_mutex(m: &Arc<Mutex<u32>>) {
        let held = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison the mutex");
        })
        .join();
    }

    fn poison_rwlock(rw: &Arc<RwLock<u32>>) {
        let held = rw.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.write();
            panic!("poison the rwlock");
        })
        .join();
    }

    #[test]
    fn a_poisoned_lock_is_recovered_once_and_counted_under_its_name() {
        let m = Arc::new(Mutex::new(1));
        poison_mutex(&m);
        assert_eq!(*TABLE.mutex(&m, "one"), 1);
        assert!(!m.is_poisoned(), "the poison is cleared");
        assert_eq!(*TABLE.mutex(&m, "one"), 1);
        assert_eq!(TABLE.count("one"), 1, "one recovery per panic");
        assert_eq!(TABLE.count("two"), 0, "counted under its own name");
        assert_eq!(TABLE.count_at(0), 1);
    }

    #[test]
    fn readers_of_a_poisoned_rwlock_count_one_recovery() {
        static RW: Recoveries<1> = Recoveries::new(["rw"]);
        let rw = Arc::new(RwLock::new(2));
        poison_rwlock(&rw);
        assert_eq!(*RW.read(&rw, "rw"), 2);
        assert_eq!(*RW.read(&rw, "rw"), 2);
        assert_eq!(RW.count("rw"), 1, "two reads, one panic, one recovery");

        poison_rwlock(&rw);
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let rw = rw.clone();
                std::thread::spawn(move || *RW.read(&rw, "rw"))
            })
            .collect();
        for r in readers {
            assert_eq!(r.join().unwrap(), 2);
        }
        assert_eq!(RW.count("rw"), 2, "four concurrent readers, one panic, one recovery");
        assert_eq!(*RW.write(&rw, "rw"), 2);
        assert!(!rw.is_poisoned());
    }
}
