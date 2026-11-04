//! The monotonic id source for branches, tags and Call-IDs. Deterministic (no
//! RNG), so report bytes are stable across runs; seeded and given a Call-ID
//! nonce where identifiers must be unique across runs.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Shared by every agent of one harness or binder.
pub(crate) struct Ids {
    counter: AtomicU64,
    /// Carried in every Call-ID minted after it is set.
    nonce: Mutex<Option<String>>,
}

impl Ids {
    pub(crate) fn new() -> Self {
        Self { counter: AtomicU64::new(1), nonce: Mutex::new(None) }
    }

    pub(crate) fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Restart the counter at `seed`.
    pub(crate) fn seed(&self, seed: u64) {
        self.counter.store(seed, Ordering::Relaxed);
    }

    /// Carry `nonce` in every Call-ID minted from now on.
    pub(crate) fn set_nonce(&self, nonce: &str) {
        *self.nonce.lock().unwrap() = Some(nonce.to_string());
    }

    /// A fresh Call-ID for agent `name` at `ip`: `<name>-<n>[.<nonce>]@<ip>`.
    pub(crate) fn call_id(&self, name: &str, ip: IpAddr) -> String {
        let n = self.next();
        self.call_id_at(name, ip, n)
    }

    /// The Call-ID agent `name` at `ip` mints when the counter stands at `n`.
    pub(crate) fn call_id_at(&self, name: &str, ip: IpAddr, n: u64) -> String {
        match self.nonce.lock().unwrap().as_deref() {
            Some(nonce) => format!("{name}-{n}.{nonce}@{ip}"),
            None => format!("{name}-{n}@{ip}"),
        }
    }

    /// Seed the counter at the first value, from the next one on, whose
    /// Call-ID for agent `name` at `ip` `accept` takes, and return that
    /// Call-ID. `None` when no value of the next `bound` does.
    pub(crate) fn seed_call_id_where(
        &self,
        name: &str,
        ip: IpAddr,
        bound: u64,
        accept: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let first = self.next();
        (first..first.saturating_add(bound)).find_map(|n| {
            let id = self.call_id_at(name, ip, n);
            accept(&id).then(|| {
                self.seed(n);
                id
            })
        })
    }
}

impl super::Agent {
    /// Seed the id counter every agent of the harness shares so that the next
    /// Call-ID this agent mints is the first one, from the counter on, that
    /// `accept` takes, and return it. The call this agent originates next
    /// carries it when no identifier is drawn in between. Panics when none of
    /// the next 100 000 values is accepted.
    pub fn seed_next_call_id(&self, accept: impl Fn(&str) -> bool) -> String {
        self.ids
            .seed_call_id_where(&self.name, self.addr.ip(), 100_000, accept)
            .unwrap_or_else(|| panic!("{}: no Call-ID of the next 100 000 is accepted", self.name))
    }
}
