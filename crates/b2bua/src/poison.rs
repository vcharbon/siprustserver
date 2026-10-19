//! The worker's named locks outside the store (ADR-0051 §7; the store's are
//! in [`crate::store`]): every other std lock of the crate is taken through
//! [`locked`], [`read`] or [`write`] under one of these names, and a poisoned
//! one is recovered and counted under it
//! (`b2bua_poisoned_lock_recoveries_total{lock}`).

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use lock_poison::Recoveries;

pub(crate) const ADMIT_BY_CALL: &str = "admit_by_call";
pub(crate) const BACKEND_GATE: &str = "backend_gate";
pub(crate) const CAPACITY_LIMITS: &str = "capacity_limits";
pub(crate) const CDR_MEMORY: &str = "cdr_memory";
pub(crate) const DISPATCH_BODIES: &str = "dispatch_bodies";
pub(crate) const DISPATCH_QUEUE: &str = "dispatch_queue";
pub(crate) const DISPATCH_SLOTS: &str = "dispatch_slots";
pub(crate) const LATE_PRACKS: &str = "late_pracks";
pub(crate) const LIMITER_BREAKER: &str = "limiter_breaker";
pub(crate) const LIMITER_REFRESH_DUE: &str = "limiter_refresh_due";
pub(crate) const LIMITER_RELEASE_WAITING: &str = "limiter_release_waiting";
pub(crate) const LIMITER_REPORTS: &str = "limiter_reports";
pub(crate) const OVERLOAD_SAMPLER: &str = "overload_sampler";
pub(crate) const OVERLOAD_SIGNAL: &str = "overload_signal";
pub(crate) const PULLER_BOOTSTRAP: &str = "puller_bootstrap";
pub(crate) const READINESS_TRANSITIONS: &str = "readiness_transitions";
pub(crate) const REAPER_LEDGERS: &str = "reaper_ledgers";
pub(crate) const SUPERVISOR_PEERS: &str = "supervisor_peers";
pub(crate) const SUPERVISOR_WIRING: &str = "supervisor_wiring";
pub(crate) const TRACE_PROCESS: &str = "trace_process";
pub(crate) const TRACE_SPANS: &str = "trace_spans";

/// Every lock name, in counter order: the `lock` label's values.
pub const LOCKS: [&str; 21] = [
    ADMIT_BY_CALL,
    BACKEND_GATE,
    CAPACITY_LIMITS,
    CDR_MEMORY,
    DISPATCH_BODIES,
    DISPATCH_QUEUE,
    DISPATCH_SLOTS,
    LATE_PRACKS,
    LIMITER_BREAKER,
    LIMITER_REFRESH_DUE,
    LIMITER_RELEASE_WAITING,
    LIMITER_REPORTS,
    OVERLOAD_SAMPLER,
    OVERLOAD_SIGNAL,
    PULLER_BOOTSTRAP,
    READINESS_TRANSITIONS,
    REAPER_LEDGERS,
    SUPERVISOR_PEERS,
    SUPERVISOR_WIRING,
    TRACE_PROCESS,
    TRACE_SPANS,
];

/// The recoveries of the worker's locks in this process.
pub static RECOVERIES: Recoveries<{ LOCKS.len() }> = Recoveries::new(LOCKS);

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
