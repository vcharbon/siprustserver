//! The per-call serialization lock (ADR-0010 X2), the second FIFO layer over
//! the dispatcher: one mutex per `callRef` while any task holds it or waits on
//! it, its map entry removed by the last guard of a call that is not resident.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

use super::{poison, CallState, Inner};

/// A call's lock entry: its mutex, and whether a guard dropping it must check
/// whether the entry goes.
///
/// `retire` holds whenever the entry exists while its call is not resident:
/// it is set under the map lock by a [`CallState::lock`] that finds the call
/// absent and by `drop_local`, and cleared by the residency inserts and by a
/// guard that finds the call resident.
pub(super) struct LockEntry {
    mutex: Arc<tokio::sync::Mutex<()>>,
    retire: AtomicBool,
}

impl LockEntry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mutex: Arc::new(tokio::sync::Mutex::new(())),
            retire: AtomicBool::new(false),
        })
    }

    /// Mark the entry for its last guard to remove.
    pub(super) fn retire(&self) {
        self.retire.store(true, Ordering::Release);
    }

    /// The call is resident again: its entry stays.
    pub(super) fn keep(&self) {
        self.retire.store(false, Ordering::Release);
    }
}

/// A holder of, or a waiter on, one call's lock.
///
/// Every holder and every waiter owns one `Arc` of the entry, cloned only under
/// the map lock, so a drop that finds the map's `Arc` and its own the only two
/// knows no task holds or waits on that mutex, and removes the entry. A later
/// `lock` then makes a fresh one: at most one mutex per `callRef` has a holder
/// or a waiter.
///
/// **Lock order:** a guard of a retired entry takes the map lock as it drops, so
/// no `CallGuard` is dropped while any std lock is held.
pub struct CallGuard {
    inner: Arc<Mutex<Inner>>,
    call_ref: String,
    entry: Arc<LockEntry>,
    held: Option<OwnedMutexGuard<()>>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        drop(self.held.take());
        if !self.entry.retire.load(Ordering::Acquire) {
            return;
        }
        let mut inner = poison::locked(&self.inner, poison::CALL_STORE);
        if inner.calls.contains_key(&self.call_ref) {
            self.entry.keep();
            return;
        }
        let ours = inner.locks.get(&self.call_ref).is_some_and(|e| Arc::ptr_eq(e, &self.entry));
        if ours && Arc::strong_count(&self.entry) == 2 {
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
        let mut guard = self.lock_entry(call_ref);
        guard.held = Some(guard.entry.mutex.clone().lock_owned().await);
        guard
    }

    /// `call_ref`'s lock when no task holds or waits on it, never waiting.
    pub fn try_lock(&self, call_ref: &str) -> Option<CallGuard> {
        let mut guard = self.lock_entry(call_ref);
        guard.held = Some(guard.entry.mutex.clone().try_lock_owned().ok()?);
        Some(guard)
    }

    /// A guard on `call_ref`'s entry, holding nothing yet. Built once the map
    /// lock is released: a guard dropped under it would take it again.
    fn lock_entry(&self, call_ref: &str) -> CallGuard {
        let entry = {
            let mut inner = self.locked();
            let resident = inner.calls.contains_key(call_ref);
            let entry = inner.locks.entry(call_ref.to_string()).or_insert_with(LockEntry::new);
            if !resident {
                entry.retire();
            }
            entry.clone()
        };
        CallGuard { inner: self.inner.clone(), call_ref: call_ref.to_string(), entry, held: None }
    }

    /// The number of live per-call lock entries. Tracks
    /// [`active_count`](Self::active_count) plus the calls a task holds or
    /// waits on the lock of while not resident; a residue after teardown is a
    /// lock leak (the `b2bua_store_locks` − `b2bua_store_calls` gap).
    pub fn lock_count(&self) -> usize {
        self.locked().locks.len()
    }
}
