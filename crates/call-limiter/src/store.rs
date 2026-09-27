//! [`CallStore`] — admission holds keyed by the call.
//!
//! The store keeps, per `call_ref`, the multiset of limiter ids the call holds
//! and the lease that bounds it, and per id the live count over every call.
//!
//! - **admit** replaces the call's set in one step, checked net of the set the
//!   call already holds: an id the call keeps or reduces never refuses, an id
//!   it adds is checked against the cap the entry states; the same id twice in
//!   one list takes two slots. All or none: a refusal leaves every count as it
//!   was and keeps the call's old set, unless `release_on_refusal` drops it in
//!   the same step.
//! - **release** drops the call's set and tombstones the `call_ref` for one
//!   lease, so an admit or a refresh landing after the call ended re-creates
//!   nothing. A release of an unknown or tombstoned call is a no-op.
//! - **refresh** extends the lease of a known call. For a call the store does
//!   not know and has not tombstoned it re-creates the set from the ids the
//!   refresh carries, with no cap check: the call exists and was admitted, and
//!   its set lapsed (a lease missed across a takeover or a restart of the
//!   store). A tombstoned call is refused.
//! - **sweep** drops every set whose lease lapsed (a release the store never
//!   received) and every tombstone past its lease, and counts the sets.
//!
//! All time is read through the injected [`Clock`], so leases advance
//! deterministically under a paused test clock; the store is pure compute.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Mutex;

use sip_clock::Clock;

use crate::wire::AdmitEntry;

/// The store's lease. The default is the deployed value; the workers refresh
/// below it by more than one period, and how the lease relates to the
/// replica TTL and a reactive takeover is stated in ADR-0038.
#[derive(Clone, Copy, Debug)]
pub struct LimiterConfig {
    /// How long a call's set lives without a refresh, and how long a released
    /// key stays tombstoned (seconds).
    pub lease_sec: i64,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self { lease_sec: 120 }
    }
}

/// The outcome of one refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshResult {
    /// The call's lease was extended.
    Extended,
    /// The store held no set for the call: it was re-created from the ids the
    /// refresh carries.
    Reregistered,
    /// The call was released within the last lease: nothing is held for it.
    Released,
}

/// The outcome of one admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitResult {
    /// The call's set is now the entries admitted.
    Admitted,
    /// An id the call adds is at its cap; nothing moved.
    Rejected {
        /// The first id found at its cap.
        limiter_id: String,
    },
    /// The call was released within the last lease: nothing is held for it.
    Released,
}

/// One call's holds: its ids as a multiset in list order, and its lease.
struct CallSet {
    ids: Vec<String>,
    lease_expires_at_ms: i64,
}

struct Inner {
    /// `call_ref` -> the call's set.
    calls: HashMap<String, CallSet>,
    /// Limiter id -> the live count over every call's set. An id nobody holds
    /// has no entry.
    counts: HashMap<String, i64>,
    /// Released `call_ref` -> when its tombstone lapses.
    tombstones: HashMap<String, i64>,
    /// Lease deadlines of the sets, oldest first. A refresh pushes a new
    /// deadline and leaves the old one stale: a popped deadline expires the
    /// set only when it is still the set's own.
    set_deadlines: BinaryHeap<Reverse<(i64, String)>>,
    /// Lapse instants of the tombstones, oldest first.
    tombstone_deadlines: BinaryHeap<Reverse<(i64, String)>>,
    lease_expired_calls: u64,
    lease_expired_holds: u64,
    reregistered_calls: u64,
    releases_total: u64,
    admits_refused_released: u64,
}

/// The keyed store. Interior-mutable so it can be shared (`Arc`) by the HTTP
/// handler and the janitor.
pub struct CallStore {
    inner: Mutex<Inner>,
    clock: Clock,
    cfg: LimiterConfig,
}

impl CallStore {
    /// Build a store over the injected clock.
    pub fn new(cfg: LimiterConfig, clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner {
                calls: HashMap::new(),
                counts: HashMap::new(),
                tombstones: HashMap::new(),
                set_deadlines: BinaryHeap::new(),
                tombstone_deadlines: BinaryHeap::new(),
                lease_expired_calls: 0,
                lease_expired_holds: 0,
                reregistered_calls: 0,
                releases_total: 0,
                admits_refused_released: 0,
            }),
            clock,
            cfg,
        }
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn lease_ms(&self) -> i64 {
        self.cfg.lease_sec * 1000
    }

    /// Replace the call's set with `entries`, checked net of its current set.
    pub fn admit(
        &self,
        call_ref: &str,
        entries: &[AdmitEntry],
        release_on_refusal: bool,
    ) -> AdmitResult {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);

        if inner.tombstones.contains_key(call_ref) {
            inner.admits_refused_released += 1;
            return AdmitResult::Released;
        }

        let mut own_old: HashMap<&str, i64> = HashMap::new();
        if let Some(set) = inner.calls.get(call_ref) {
            for id in &set.ids {
                *own_old.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        let mut own_new: HashMap<&str, i64> = HashMap::new();
        for e in entries {
            *own_new.entry(e.id.as_str()).or_insert(0) += 1;
        }

        // Every entry is checked against its own cap with the whole list's
        // effect on its id; only an id the call adds slots to can refuse.
        for e in entries {
            let old = own_old.get(e.id.as_str()).copied().unwrap_or(0);
            let new = own_new[e.id.as_str()];
            if new <= old {
                continue;
            }
            let count = inner.counts.get(&e.id).copied().unwrap_or(0);
            if count - old + new > e.limit {
                if release_on_refusal {
                    drop_set(&mut inner, call_ref);
                }
                return AdmitResult::Rejected { limiter_id: e.id.clone() };
            }
        }

        drop_set(&mut inner, call_ref);
        let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        insert_set(&mut inner, call_ref, ids, now_ms + self.lease_ms());
        AdmitResult::Admitted
    }

    /// Drop the call's set and tombstone the `call_ref` for one lease. A
    /// second release, or one of a call never admitted, changes nothing.
    pub fn release(&self, call_ref: &str) {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        inner.releases_total += 1;
        if inner.tombstones.contains_key(call_ref) {
            return;
        }
        drop_set(&mut inner, call_ref);
        let lapses_at_ms = now_ms + self.lease_ms();
        inner.tombstones.insert(call_ref.to_string(), lapses_at_ms);
        inner.tombstone_deadlines.push(Reverse((lapses_at_ms, call_ref.to_string())));
    }

    /// Extend the call's lease, or re-create its set from `ids` (no cap check)
    /// when the store holds none and the call is not tombstoned.
    pub fn refresh(&self, call_ref: &str, ids: &[String]) -> RefreshResult {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        if inner.tombstones.contains_key(call_ref) {
            return RefreshResult::Released;
        }
        let lease_expires_at_ms = now_ms + self.lease_ms();
        if let Some(set) = inner.calls.get_mut(call_ref) {
            set.lease_expires_at_ms = lease_expires_at_ms;
            inner.set_deadlines.push(Reverse((lease_expires_at_ms, call_ref.to_string())));
            return RefreshResult::Extended;
        }
        insert_set(&mut inner, call_ref, ids.to_vec(), lease_expires_at_ms);
        inner.reregistered_calls += 1;
        RefreshResult::Reregistered
    }

    /// Drop every lapsed set and tombstone now (the janitor entry point).
    /// Returns how many sets lapsed.
    pub fn sweep_now(&self) -> u64 {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        let before = inner.lease_expired_calls;
        sweep(&mut inner, now_ms);
        inner.lease_expired_calls - before
    }

    /// The live count `id` holds over every call's set.
    pub fn held(&self, id: &str) -> i64 {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        inner.counts.get(id).copied().unwrap_or(0)
    }

    /// The number of calls holding a set.
    pub fn calls(&self) -> usize {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        inner.calls.len()
    }

    /// The gauges and cumulative counters, for metrics.
    pub fn stats(&self) -> StoreStats {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        StoreStats {
            calls: inner.calls.len() as u64,
            current_total: inner.counts.values().sum(),
            admission_max: inner.counts.values().copied().max().unwrap_or(0),
            tombstones: inner.tombstones.len() as u64,
            lease_expired_calls: inner.lease_expired_calls,
            lease_expired_holds: inner.lease_expired_holds,
            reregistered_calls: inner.reregistered_calls,
            releases_total: inner.releases_total,
            admits_refused_released: inner.admits_refused_released,
        }
    }
}

/// A snapshot of the store's metric-relevant numbers.
#[derive(Clone, Copy, Debug)]
pub struct StoreStats {
    /// Calls holding a set.
    pub calls: u64,
    /// Sum of every live count over the ids.
    pub current_total: i64,
    /// The largest live count of one id: what the next admit of that id
    /// compares with its cap. 0 when nothing is held.
    pub admission_max: i64,
    /// Released calls still tombstoned.
    pub tombstones: u64,
    /// Cumulative sets dropped because their lease lapsed.
    pub lease_expired_calls: u64,
    /// Cumulative holds those sets carried.
    pub lease_expired_holds: u64,
    /// Cumulative sets re-created by a refresh of a call the store no longer
    /// held.
    pub reregistered_calls: u64,
    /// Cumulative release calls, no-ops included.
    pub releases_total: u64,
    /// Cumulative admits refused because the call was released.
    pub admits_refused_released: u64,
}

/// Count `ids` for `call_ref` under a lease ending at `lease_expires_at_ms`;
/// an empty list registers nothing.
fn insert_set(inner: &mut Inner, call_ref: &str, ids: Vec<String>, lease_expires_at_ms: i64) {
    if ids.is_empty() {
        return;
    }
    for id in &ids {
        *inner.counts.entry(id.clone()).or_insert(0) += 1;
    }
    inner.calls.insert(call_ref.to_string(), CallSet { ids, lease_expires_at_ms });
    inner.set_deadlines.push(Reverse((lease_expires_at_ms, call_ref.to_string())));
}

/// Remove the call's set, if any, and its holds from the counts.
fn drop_set(inner: &mut Inner, call_ref: &str) -> usize {
    let Some(set) = inner.calls.remove(call_ref) else {
        return 0;
    };
    for id in &set.ids {
        if let Some(count) = inner.counts.get_mut(id) {
            *count -= 1;
            if *count <= 0 {
                inner.counts.remove(id);
            }
        }
    }
    set.ids.len()
}

/// Drop every set whose lease lapsed and every tombstone past its lease.
fn sweep(inner: &mut Inner, now_ms: i64) {
    while let Some(Reverse((deadline, _))) = inner.set_deadlines.peek() {
        if *deadline > now_ms {
            break;
        }
        let Some(Reverse((deadline, call_ref))) = inner.set_deadlines.pop() else {
            break;
        };
        // Stale when a refresh moved the set's lease past this deadline.
        let lapsed = inner.calls.get(&call_ref).is_some_and(|s| s.lease_expires_at_ms == deadline);
        if lapsed {
            let holds = drop_set(inner, &call_ref);
            inner.lease_expired_calls += 1;
            inner.lease_expired_holds += holds as u64;
        }
    }
    while let Some(Reverse((deadline, _))) = inner.tombstone_deadlines.peek() {
        if *deadline > now_ms {
            break;
        }
        let Some(Reverse((deadline, call_ref))) = inner.tombstone_deadlines.pop() else {
            break;
        };
        if inner.tombstones.get(&call_ref) == Some(&deadline) {
            inner.tombstones.remove(&call_ref);
        }
    }
}
