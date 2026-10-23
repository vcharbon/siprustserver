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
        match self.nonce.lock().unwrap().as_deref() {
            Some(nonce) => format!("{name}-{n}.{nonce}@{ip}"),
            None => format!("{name}-{n}@{ip}"),
        }
    }
}
