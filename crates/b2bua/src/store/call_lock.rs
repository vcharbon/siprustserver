//! The per-call serialization lock (ADR-0010 X2), the second FIFO layer over
//! the dispatcher: one mutex per `callRef` while any task holds it or waits on
//! it, its map entry removed by the last guard of a call that is not resident.

use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

use super::{poison, CallState, Inner};

/// A call's lock entry: its mutex, and whether a guard dropping it must check
/// whether the entry goes.
///
/// `retire` holds whenever the entry exists while its call is not resident:
/// it is set under the map lock by a [`CallState::lock`] that finds the call
/// absent and by `drop_local`, each time by a task that owns a guard and so
/// checks at its own drop, and cleared by the residency inserts and by a
/// guard that finds the call resident. The flag has its own allocation, so a
/// guard reads it after letting its entry go.
pub(super) struct LockEntry {
    mutex: Arc<tokio::sync::Mutex<()>>,
    retire: Arc<AtomicBool>,
}

impl LockEntry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mutex: Arc::new(tokio::sync::Mutex::new(())),
            retire: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Mark the entry for its last guard to remove. The fence pairs with a
    /// guard's on its fast path: the guard sees the mark, or the marker's
    /// later count sees the guard's entry gone.
    pub(super) fn retire(&self) {
        self.retire.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// The call is resident again: its entry stays.
    pub(super) fn keep(&self) {
        self.retire.store(false, Ordering::SeqCst);
    }
}

/// A holder of, or a waiter on, one call's lock.
///
/// Every holder and every waiter owns one `Arc` of the entry, cloned only under
/// the map lock. A guard of a retired entry lets its `Arc` go under the map
/// lock, and removes the entry when the map's is then the only one: no task
/// holds or waits on that mutex. A later `lock` makes a fresh one, so at most
/// one mutex per `callRef` has a holder or a waiter, and the last guard of a
/// call not resident leaves no entry.
///
/// **Lock order:** a guard of a retired entry takes the map lock as it drops, so
/// no `CallGuard` is dropped while any std lock is held.
pub struct CallGuard {
    inner: Arc<Mutex<Inner>>,
    call_ref: String,
    entry: Option<Arc<LockEntry>>,
    retire: Arc<AtomicBool>,
    held: Option<OwnedMutexGuard<()>>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        drop(self.held.take());
        if !self.retire.load(Ordering::SeqCst) {
            // Not retired: the entry goes first, then the mark is read again
            // past the fence `LockEntry::retire` pairs with.
            drop(self.entry.take());
            fence(Ordering::SeqCst);
            if !self.retire.load(Ordering::SeqCst) {
                return;
            }
        }
        let mut inner = poison::locked(&self.inner, poison::CALL_STORE);
        drop(self.entry.take());
        let resident = inner.calls.contains_key(&self.call_ref);
        let Some(entry) = inner.locks.get(&self.call_ref) else { return };
        if !Arc::ptr_eq(&entry.retire, &self.retire) {
            return;
        }
        if resident {
            entry.keep();
        } else if Arc::strong_count(entry) == 1 {
            inner.locks.remove(&self.call_ref);
            super::shrink_idle(&mut inner.locks);
        }
    }
}

impl Inner {
    /// The call `call_ref` leaves the live map: its lock entry goes now when no
    /// task holds or waits on it, else with the last guard.
    pub(super) fn retire_lock(&mut self, call_ref: &str) {
        let Some(entry) = self.locks.get(call_ref) else { return };
        if Arc::strong_count(entry) == 1 {
            self.locks.remove(call_ref);
        } else {
            entry.retire();
        }
    }

    /// The call `call_ref` entered the live map under its lock: the entry stays.
    pub(super) fn keep_lock(&self, call_ref: &str) {
        if let Some(entry) = self.locks.get(call_ref) {
            entry.keep();
        }
    }
}

impl CallState {
    /// Acquire `call_ref`'s lock, held across a handler run.
    pub async fn lock(&self, call_ref: &str) -> CallGuard {
        let (mut guard, mutex) = self.lock_entry(call_ref);
        guard.held = Some(mutex.lock_owned().await);
        guard
    }

    /// `call_ref`'s lock when no task holds or waits on it, never waiting.
    pub fn try_lock(&self, call_ref: &str) -> Option<CallGuard> {
        let (mut guard, mutex) = self.lock_entry(call_ref);
        guard.held = Some(mutex.try_lock_owned().ok()?);
        Some(guard)
    }

    /// A guard on `call_ref`'s entry, holding nothing yet, and the entry's
    /// mutex. Built once the map lock is released: a guard dropped under it
    /// would take it again.
    fn lock_entry(&self, call_ref: &str) -> (CallGuard, Arc<tokio::sync::Mutex<()>>) {
        let entry = {
            let mut inner = self.locked();
            let resident = inner.calls.contains_key(call_ref);
            let entry = inner.locks.entry(call_ref.to_string()).or_insert_with(LockEntry::new);
            if !resident {
                entry.retire();
            }
            entry.clone()
        };
        let (mutex, retire) = (entry.mutex.clone(), entry.retire.clone());
        let call_ref = call_ref.to_string();
        let guard = CallGuard {
            inner: self.inner.clone(),
            call_ref,
            entry: Some(entry),
            retire,
            held: None,
        };
        (guard, mutex)
    }

    /// The number of live per-call lock entries. Tracks
    /// [`active_count`](Self::active_count) plus the calls a task holds or
    /// waits on the lock of while not resident; a residue after teardown is a
    /// lock leak (the `b2bua_store_locks` − `b2bua_store_calls` gap).
    pub fn lock_count(&self) -> usize {
        self.locked().locks.len()
    }
}
