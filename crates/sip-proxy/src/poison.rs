//! The proxy's named locks (ADR-0051 §7): every std lock of the crate is
//! taken through [`locked`], [`read`] or [`write`] under one of these names,
//! and a poisoned one is recovered and counted under it
//! (`sip_proxy_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use lock_poison::Recoveries;

pub(crate) const INVITE_BRANCH_LRU: &str = "invite_branch_lru";
pub(crate) const REFUSALS_MEMO: &str = "refusals_memo";
pub(crate) const LOAD_OBSERVER: &str = "load_observer";
pub(crate) const SELF_GATE: &str = "self_gate";
pub(crate) const RESOLVER_IN_FLIGHT: &str = "resolver_in_flight";
pub(crate) const RESOLVER_PINS: &str = "resolver_pins";
pub(crate) const RESOLVER_REFRESHES: &str = "resolver_refreshes";
pub(crate) const RESOLVER_CACHE: &str = "resolver_cache";
pub(crate) const TRACE_REGISTRY: &str = "trace_registry";
pub(crate) const UDP_SHARD_STATS: &str = "udp_shard_stats";
pub(crate) const SIMULATED_REGISTRY: &str = "simulated_registry";
pub(crate) const PARSE_REFUSAL_CAP: &str = "parse_refusal_cap";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 12] = [
    INVITE_BRANCH_LRU,
    REFUSALS_MEMO,
    LOAD_OBSERVER,
    SELF_GATE,
    RESOLVER_IN_FLIGHT,
    RESOLVER_PINS,
    RESOLVER_REFRESHES,
    RESOLVER_CACHE,
    TRACE_REGISTRY,
    UDP_SHARD_STATS,
    SIMULATED_REGISTRY,
    PARSE_REFUSAL_CAP,
];

/// The recoveries of the proxy's locks in this process.
pub(crate) static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

/// Lock `m`, the lock `name`.
pub(crate) fn locked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    RECOVERIES.mutex(m, name)
}

/// Read-lock `rw`, the lock `name`.
pub(crate) fn read<'a, T>(rw: &'a RwLock<T>, name: &'static str) -> RwLockReadGuard<'a, T> {
    RECOVERIES.read(rw, name)
}

/// Write-lock `rw`, the lock `name`.
pub(crate) fn write<'a, T>(rw: &'a RwLock<T>, name: &'static str) -> RwLockWriteGuard<'a, T> {
    RECOVERIES.write(rw, name)
}
