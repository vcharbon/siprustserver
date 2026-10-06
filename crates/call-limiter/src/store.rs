//! [`CallStore`] — admission holds keyed by the call.
//!
//! The store keeps, per `key`, the entries the call holds (a multiset of
//! limiter ids, each with the cap it was admitted under), the change number
//! of the last admit it answered for the key, and the lease that bounds the
//! set; per id, the live count over every call. The key is the client's
//! per-call limiter key, unique over time.
//!
//! - **admit** replaces the call's set in one step, checked net of the set the
//!   call already holds: an id the call keeps or reduces never refuses, an id
//!   it adds is checked against the cap the entry states; the same id twice in
//!   one list takes two slots. All or none: a refusal leaves every count as it
//!   was and keeps the call's old set, unless `release_on_refusal` drops it in
//!   the same step. An admit whose change number is not above the one the
//!   store knows for the key is superseded and changes nothing; every
//!   answered admit records its number (on the key's set, its drop fence, or
//!   else a change marker kept one lease that fences nothing), so a late admit
//!   never overwrites a newer one. An admit that drops
//!   the set without replacing it (a cap refusal with `release_on_refusal`, an
//!   empty replacement) fences the key against refresh for one lease, until
//!   the next admit of the key: a refresh that left before the drop re-creates
//!   nothing. An admit carries the call's held set: for a key holding no set
//!   and not fenced (a marker is no fence) it first re-creates that set as a
//!   refresh would, so the change is checked net of what the call holds
//!   whether or not the store restarted. Every answer but a release fence's
//!   states the set held after it.
//! - **release** names one or more calls; it drops each call's set and fences
//!   its `key` for one lease against admit and refresh, so an admit or a
//!   refresh landing after the call ended re-creates nothing. A release of an
//!   unknown or released call changes no count and creates no set.
//! - **refresh** names one or more calls, each answered on its own terms and
//!   stating the set held for it. It extends the lease of a known call. For a
//!   call the store does not know and has not fenced it re-creates the set
//!   from the entries and change number the refresh carries, with no cap
//!   check: the call exists and was admitted, and its set lapsed (a lease
//!   missed across a takeover or a restart of the store). A fenced call is
//!   refused, and the answer names the fence: an admit dropped the set (the
//!   call holds nothing by its own request) or the call was released.
//! - **sweep** drops every set whose lease lapsed (a release the store never
//!   received) and every fence past its lease, and counts the sets.
//!
//! All time is read through the injected [`Clock`], so leases advance
//! deterministically under a paused test clock; the store is pure compute.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Mutex;

use sip_clock::Clock;

use crate::wire::{AdmitEntry, HeldSet};

/// The lease a store runs unless configured otherwise (seconds).
pub const DEFAULT_LEASE_SEC: i64 = 120;

/// The longest lease a store runs (seconds): one day.
pub const MAX_LEASE_SEC: i64 = 86_400;

/// The store's lease. The default is the deployed value; every admit and
/// refresh answer states it, the workers refresh below it by more than one
/// period, and how the lease relates to the replica TTL and a reactive
/// takeover is stated in ADR-0040.
#[derive(Clone, Copy, Debug)]
pub struct LimiterConfig {
    /// How long a call's set lives without a refresh, and how long a key
    /// stays fenced (seconds).
    pub lease_sec: i64,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self { lease_sec: DEFAULT_LEASE_SEC }
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
    /// Nothing is held for the call and nothing was re-created: the call was
    /// released within the last lease, or the refresh carried no ids.
    Released,
    /// Nothing is held for the call and nothing was re-created: an admit of
    /// the key dropped its set without replacing it, and no admit since
    /// replaced it.
    Dropped,
}

/// Why a key is fenced against refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fence {
    /// The call was released: an admit of the key is refused too, for one
    /// lease.
    Released,
    /// An admit numbered `change` dropped the set without replacing it: the
    /// next admit of the key above that number clears the fence.
    Dropped { change: u64 },
}

/// The outcome of one admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitResult {
    /// The call's set is now the entries admitted, under the admit's number.
    Admitted,
    /// An id the call adds is at its cap; nothing moved but the drop
    /// `release_on_refusal` asked for.
    Rejected {
        /// The first id found at its cap.
        limiter_id: String,
        /// The set held for the key after the refusal.
        held: HeldSet,
    },
    /// The admit's change number is not above the one the store knows for
    /// the key; nothing moved.
    Superseded {
        /// The set held for the key.
        held: HeldSet,
    },
    /// The call was released within the last lease: nothing is held for it.
    Released,
}

/// The outcome of one refresh, and the set held for the key after it
/// (`None` when the call was released).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refreshed {
    /// What the refresh did.
    pub result: RefreshResult,
    /// The set held for the key.
    pub held: Option<HeldSet>,
}

/// One call's holds: its entries as a multiset in list order, the change
/// number they were admitted (or last refused, or re-registered) under, and
/// its lease.
struct CallSet {
    entries: Vec<AdmitEntry>,
    change: u64,
    lease_expires_at_ms: i64,
}

struct Inner {
    /// `key` -> the call's set.
    calls: HashMap<String, CallSet>,
    /// Limiter id -> the live count over every call's set. An id nobody holds
    /// has no entry.
    counts: HashMap<String, i64>,
    /// Fenced `key` -> why, and when the fence lapses.
    fences: HashMap<String, (Fence, i64)>,
    /// Lease deadlines of the sets, oldest first. A refresh pushes a new
    /// deadline and leaves the old one stale: a popped deadline expires the
    /// set only when it is still the set's own.
    set_deadlines: BinaryHeap<Reverse<(i64, String)>>,
    /// `key` -> the number of the last admit answered for a key holding no
    /// set and no drop fence, and when the marker lapses. It orders admits and
    /// fences nothing.
    markers: HashMap<String, (u64, i64)>,
    /// Lapse instants of the markers, oldest first.
    marker_deadlines: BinaryHeap<Reverse<(i64, String)>>,
    /// Lapse instants of the fences, oldest first.
    fence_deadlines: BinaryHeap<Reverse<(i64, String)>>,
    lease_expired_calls: u64,
    lease_expired_holds: u64,
    reregistered_calls: u64,
    admit_reregistered_calls: u64,
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
                fences: HashMap::new(),
                markers: HashMap::new(),
                marker_deadlines: BinaryHeap::new(),
                set_deadlines: BinaryHeap::new(),
                fence_deadlines: BinaryHeap::new(),
                lease_expired_calls: 0,
                lease_expired_holds: 0,
                reregistered_calls: 0,
                admit_reregistered_calls: 0,
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

    /// The store's lease, milliseconds.
    pub fn lease_ms(&self) -> i64 {
        self.cfg.lease_sec.saturating_mul(1000)
    }

    /// [`admit_carrying`](Self::admit_carrying) for a call that carries no
    /// held set: it learnt of none.
    pub fn admit(
        &self,
        key: &str,
        change: u64,
        entries: &[AdmitEntry],
        release_on_refusal: bool,
    ) -> AdmitResult {
        self.admit_carrying(key, change, &HeldSet::default(), entries, release_on_refusal)
    }

    /// Replace the call's set with `entries` under the call's `change`
    /// number, checked net of its current set. `held` is the set the call
    /// last learnt the store holds for it: when the store holds no set for
    /// the key and has not fenced it, an admit that is not superseded first
    /// re-creates `held` with no cap check, exactly as a refresh carrying it
    /// would, then checks the change against it. The admit's own number
    /// replaces the re-created set's in every outcome.
    pub fn admit_carrying(
        &self,
        key: &str,
        change: u64,
        held: &HeldSet,
        entries: &[AdmitEntry],
        release_on_refusal: bool,
    ) -> AdmitResult {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);

        if inner.fences.get(key).is_some_and(|(fence, _)| *fence == Fence::Released) {
            inner.admits_refused_released += 1;
            return AdmitResult::Released;
        }
        if known_change(&inner, key).is_some_and(|known| change <= known) {
            return AdmitResult::Superseded { held: held_set(&inner, key) };
        }
        // A key with no set and no fence lost the call's set (a lapse, a
        // restart): re-create it as a refresh carrying `held` would, and
        // check the change net of it.
        if reregisters(&inner, key, &held.entries) {
            reregister(&mut inner, key, held.change, &held.entries, now_ms + self.lease_ms());
            inner.admit_reregistered_calls += 1;
        }

        let mut own_old: HashMap<&str, i64> = HashMap::new();
        if let Some(set) = inner.calls.get(key) {
            for e in &set.entries {
                *own_old.entry(e.id.as_str()).or_insert(0) += 1;
            }
        }
        let mut own_new: HashMap<&str, i64> = HashMap::new();
        for e in entries {
            *own_new.entry(e.id.as_str()).or_insert(0) += 1;
        }

        // Every entry is checked against its own cap with the whole list's
        // effect on its id; only an id the call adds slots to can refuse.
        let refused = entries.iter().find(|e| {
            let old = own_old.get(e.id.as_str()).copied().unwrap_or(0);
            let new = own_new[e.id.as_str()];
            new > old && inner.counts.get(&e.id).copied().unwrap_or(0) - old + new > e.limit
        });
        if let Some(e) = refused {
            let limiter_id = e.id.clone();
            if release_on_refusal {
                drop_set(&mut inner, key);
                inner.markers.remove(key);
                fence(&mut inner, key, Fence::Dropped { change }, now_ms + self.lease_ms());
            } else {
                record_change(&mut inner, key, change, now_ms + self.lease_ms());
            }
            return AdmitResult::Rejected { limiter_id, held: held_set(&inner, key) };
        }

        drop_set(&mut inner, key);
        inner.fences.remove(key);
        inner.markers.remove(key);
        let lease_expires_at_ms = now_ms + self.lease_ms();
        if entries.is_empty() {
            fence(&mut inner, key, Fence::Dropped { change }, lease_expires_at_ms);
        } else {
            insert_set(&mut inner, key, entries.to_vec(), change, lease_expires_at_ms);
        }
        AdmitResult::Admitted
    }

    /// Release every call of `keys` in one step: drop each call's set and
    /// fence its key for one lease against admit and refresh. A second
    /// release of a key changes nothing; a release of a call never admitted,
    /// or whose set an admit dropped, fences its key the same.
    pub fn release<K: AsRef<str>>(&self, keys: &[K]) {
        let now_ms = self.now_ms();
        let fenced_until_ms = now_ms + self.lease_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        for key in keys {
            let key = key.as_ref();
            inner.releases_total += 1;
            if inner.fences.get(key).is_some_and(|(fence, _)| *fence == Fence::Released) {
                continue;
            }
            drop_set(&mut inner, key);
            inner.markers.remove(key);
            fence(&mut inner, key, Fence::Released, fenced_until_ms);
        }
    }

    /// Extend the call's lease, or re-create its set from `entries` under
    /// `change` (no cap check) when the store holds none and the key is not
    /// fenced.
    pub fn refresh(&self, key: &str, change: u64, entries: &[AdmitEntry]) -> Refreshed {
        self.refresh_all([(key, change, entries)]).pop().expect("one call, one outcome")
    }

    /// [`refresh`](Self::refresh) every call of `calls` in one step: one
    /// outcome per call, in order, each exactly what its own refresh answers.
    pub fn refresh_all<'a>(
        &self,
        calls: impl IntoIterator<Item = (&'a str, u64, &'a [AdmitEntry])>,
    ) -> Vec<Refreshed> {
        let now_ms = self.now_ms();
        let lease_expires_at_ms = now_ms + self.lease_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        calls
            .into_iter()
            .map(|(key, change, entries)| {
                refresh(&mut inner, key, change, entries, lease_expires_at_ms)
            })
            .collect()
    }

    /// The set the store holds for `key` (empty entries and change 0 for a key
    /// it knows nothing of; `None` for a released key).
    pub fn held_set(&self, key: &str) -> Option<HeldSet> {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        if inner.fences.get(key).is_some_and(|(fence, _)| *fence == Fence::Released) {
            return None;
        }
        Some(held_set(&inner, key))
    }

    /// Drop every lapsed set and fence now (the janitor entry point).
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
            fences: inner.fences.len() as u64,
            change_markers: inner.markers.len() as u64,
            lease_expired_calls: inner.lease_expired_calls,
            lease_expired_holds: inner.lease_expired_holds,
            reregistered_calls: inner.reregistered_calls,
            admit_reregistered_calls: inner.admit_reregistered_calls,
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
    /// Keys fenced against refresh: released calls, and calls whose set an
    /// admit dropped.
    pub fences: u64,
    /// Keys holding no set whose last admit number is kept to order later
    /// admits.
    pub change_markers: u64,
    /// Cumulative sets dropped because their lease lapsed.
    pub lease_expired_calls: u64,
    /// Cumulative holds those sets carried.
    pub lease_expired_holds: u64,
    /// Cumulative sets re-created by a refresh of a call the store no longer
    /// held.
    pub reregistered_calls: u64,
    /// Cumulative sets re-created by an admit, from the held set it carried,
    /// for a key the store no longer held.
    pub admit_reregistered_calls: u64,
    /// Cumulative release calls, no-ops included.
    pub releases_total: u64,
    /// Cumulative admits refused because the call was released.
    pub admits_refused_released: u64,
}

/// Refresh one call: extend its lease to `lease_expires_at_ms`, or re-create
/// its set from `entries` under `change` when the store holds none and the
/// key is not fenced.
fn refresh(
    inner: &mut Inner,
    key: &str,
    change: u64,
    entries: &[AdmitEntry],
    lease_expires_at_ms: i64,
) -> Refreshed {
    let answered = |inner: &Inner, result| Refreshed { result, held: Some(held_set(inner, key)) };
    if let Some(set) = inner.calls.get_mut(key) {
        set.lease_expires_at_ms = lease_expires_at_ms;
        inner.set_deadlines.push(Reverse((lease_expires_at_ms, key.to_string())));
        return answered(inner, RefreshResult::Extended);
    }
    if reregisters(inner, key, entries) {
        reregister(inner, key, change, entries, lease_expires_at_ms);
        inner.reregistered_calls += 1;
        return answered(inner, RefreshResult::Reregistered);
    }
    match inner.fences.get(key) {
        Some((Fence::Dropped { .. }, _)) => answered(inner, RefreshResult::Dropped),
        _ => Refreshed { result: RefreshResult::Released, held: None },
    }
}

/// Whether an admit or a refresh of `key` carrying `entries` (the call's held
/// set) re-creates its set: the store holds none, has not fenced the key, and
/// `entries` names an id.
fn reregisters(inner: &Inner, key: &str, entries: &[AdmitEntry]) -> bool {
    !entries.is_empty() && !inner.calls.contains_key(key) && !inner.fences.contains_key(key)
}

/// Re-create `key`'s set from `entries` under `change`, with no cap check,
/// for a lease ending at `lease_expires_at_ms`. A marker's number is the
/// key's latest: the re-created set keeps it.
fn reregister(
    inner: &mut Inner,
    key: &str,
    change: u64,
    entries: &[AdmitEntry],
    lease_expires_at_ms: i64,
) {
    let change = inner.markers.remove(key).map_or(change, |(known, _)| known.max(change));
    insert_set(inner, key, entries.to_vec(), change, lease_expires_at_ms);
}

/// The change number the store knows for `key`: its set's, its drop
/// fence's, or its marker's.
fn known_change(inner: &Inner, key: &str) -> Option<u64> {
    if let Some(set) = inner.calls.get(key) {
        return Some(set.change);
    }
    match inner.fences.get(key) {
        Some((Fence::Dropped { change }, _)) => Some(*change),
        _ => inner.markers.get(key).map(|(change, _)| *change),
    }
}

/// Record `change` as the number the store knows for `key`: an answered
/// admit that left the set as it was still orders later admits after it. A
/// key with neither set nor drop fence keeps it on a marker lapsing at
/// `lapses_at_ms`.
fn record_change(inner: &mut Inner, key: &str, change: u64, lapses_at_ms: i64) {
    if let Some(set) = inner.calls.get_mut(key) {
        set.change = change;
    } else if let Some((Fence::Dropped { change: known }, _)) = inner.fences.get_mut(key) {
        *known = change;
    } else {
        inner.markers.insert(key.to_string(), (change, lapses_at_ms));
        inner.marker_deadlines.push(Reverse((lapses_at_ms, key.to_string())));
    }
}

/// The set held for `key`, stated under the number the store knows for it.
fn held_set(inner: &Inner, key: &str) -> HeldSet {
    let entries = inner.calls.get(key).map(|set| set.entries.clone()).unwrap_or_default();
    HeldSet { change: known_change(inner, key).unwrap_or(0), entries }
}

/// Fence `key` against refresh as `why`, until `lapses_at_ms`.
fn fence(inner: &mut Inner, key: &str, why: Fence, lapses_at_ms: i64) {
    inner.fences.insert(key.to_string(), (why, lapses_at_ms));
    inner.fence_deadlines.push(Reverse((lapses_at_ms, key.to_string())));
}

/// Count `entries` (not empty) for `key` under `change` and a lease ending
/// at `lease_expires_at_ms`.
fn insert_set(
    inner: &mut Inner,
    key: &str,
    entries: Vec<AdmitEntry>,
    change: u64,
    lease_expires_at_ms: i64,
) {
    for e in &entries {
        *inner.counts.entry(e.id.clone()).or_insert(0) += 1;
    }
    inner.calls.insert(key.to_string(), CallSet { entries, change, lease_expires_at_ms });
    inner.set_deadlines.push(Reverse((lease_expires_at_ms, key.to_string())));
}

/// Remove the call's set, if any, and its holds from the counts.
fn drop_set(inner: &mut Inner, key: &str) -> usize {
    let Some(set) = inner.calls.remove(key) else {
        return 0;
    };
    for e in &set.entries {
        if let Some(count) = inner.counts.get_mut(&e.id) {
            *count -= 1;
            if *count <= 0 {
                inner.counts.remove(&e.id);
            }
        }
    }
    set.entries.len()
}

/// Drop every set whose lease lapsed and every fence and marker past its
/// lease.
fn sweep(inner: &mut Inner, now_ms: i64) {
    while let Some(Reverse((deadline, _))) = inner.set_deadlines.peek() {
        if *deadline > now_ms {
            break;
        }
        let Some(Reverse((deadline, key))) = inner.set_deadlines.pop() else {
            break;
        };
        // Stale when a refresh moved the set's lease past this deadline.
        let lapsed = inner.calls.get(&key).is_some_and(|s| s.lease_expires_at_ms == deadline);
        if lapsed {
            let holds = drop_set(inner, &key);
            inner.lease_expired_calls += 1;
            inner.lease_expired_holds += holds as u64;
        }
    }
    while let Some(Reverse((deadline, _))) = inner.fence_deadlines.peek() {
        if *deadline > now_ms {
            break;
        }
        let Some(Reverse((deadline, key))) = inner.fence_deadlines.pop() else {
            break;
        };
        if inner.fences.get(&key).is_some_and(|(_, at)| *at == deadline) {
            inner.fences.remove(&key);
        }
    }
    while let Some(Reverse((deadline, _))) = inner.marker_deadlines.peek() {
        if *deadline > now_ms {
            break;
        }
        let Some(Reverse((deadline, key))) = inner.marker_deadlines.pop() else {
            break;
        };
        if inner.markers.get(&key).is_some_and(|(_, at)| *at == deadline) {
            inner.markers.remove(&key);
        }
    }
}
