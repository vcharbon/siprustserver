//! Departed-address tombstones — the addresses that left the worker set stay
//! resolvable as `Dead` for a bounded window.
//!
//! A worker that leaves membership (or moves to a new host) can still be bound
//! and still answering: its in-flight INVITE server transactions live on until
//! Timer H. A response arriving from such an address must be recognised as
//! coming from a worker the pool no longer routes to, so the response path
//! reverse-fails it to the cookie's backup instead of relaying it back to a node
//! that has left. Tombstones are keyed by **address**, never by ordinal, so
//! ordinal resolution is untouched: a departed ordinal still resolves to `None`,
//! and a same-ordinal replacement at a new address resolves to its live entry.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::addr::ProxyAddr;

use super::{WorkerEntry, WorkerHealth, WorkerId};

/// How long a departed address keeps answering `Dead`. Timer H = 64·T1 = 32 s,
/// the longest an INVITE server transaction at that address can still produce a
/// response (RFC 3261 §17.2.1).
pub(crate) const DEPARTED_ADDRESS_TTL_MS: u64 = 32_000;

/// One departed address: the ordinal it was last known as, and when it stops
/// being resolvable.
#[derive(Debug, Clone)]
struct Tombstone {
    id: WorkerId,
    expires_at_ms: u64,
}

/// The departed-address overlay. Lock-free reads via [`ArcSwap`]; writes go
/// through `rcu` so a concurrent membership reconcile and expiry cannot erase
/// each other.
#[derive(Default)]
pub(crate) struct Tombstones {
    records: ArcSwap<HashMap<ProxyAddr, Tombstone>>,
}

impl Tombstones {
    fn update(&self, f: impl Fn(&mut HashMap<ProxyAddr, Tombstone>)) {
        self.records.rcu(|cur| {
            let mut next = (**cur).clone();
            f(&mut next);
            next
        });
    }

    /// Tombstone `addr`, last served by `id`, for [`DEPARTED_ADDRESS_TTL_MS`].
    /// Re-tombstoning an address re-arms its window.
    pub(crate) fn insert(&self, addr: ProxyAddr, id: WorkerId, now_ms: u64) {
        let expires_at_ms = now_ms.saturating_add(DEPARTED_ADDRESS_TTL_MS);
        self.update(|recs| {
            recs.insert(addr.clone(), Tombstone { id: id.clone(), expires_at_ms });
        });
    }

    /// Forget `addr` — a worker joined there, so the live entry is the truth.
    /// No-op when the address carries no tombstone.
    pub(crate) fn clear(&self, addr: &ProxyAddr) {
        if !self.records.load().contains_key(addr) {
            return;
        }
        self.update(|recs| {
            recs.remove(addr);
        });
    }

    /// Drop every tombstone whose window has closed. No-op unless one has.
    pub(crate) fn prune(&self, now_ms: u64) {
        if !self.records.load().values().any(|t| t.expires_at_ms <= now_ms) {
            return;
        }
        self.update(|recs| {
            recs.retain(|_, t| t.expires_at_ms > now_ms);
        });
    }

    /// The departed worker last bound at `addr`, as a `Dead` entry, while its
    /// window is open. `None` once the window closes.
    pub(crate) fn lookup(&self, addr: &ProxyAddr, now_ms: u64) -> Option<WorkerEntry> {
        let recs: Arc<HashMap<ProxyAddr, Tombstone>> = self.records.load_full();
        let t = recs.get(addr)?;
        if t.expires_at_ms <= now_ms {
            return None;
        }
        Some(WorkerEntry {
            id: t.id.clone(),
            address: addr.clone(),
            health: WorkerHealth::Dead,
            draining_since: None,
            first_seen_at_ms: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> ProxyAddr {
        ProxyAddr::new("10.0.0.1", 5060)
    }

    #[test]
    fn a_tombstoned_address_reads_dead_until_its_window_closes() {
        let tombs = Tombstones::default();
        tombs.insert(addr(), "w0".to_string(), 1_000);
        let entry = tombs.lookup(&addr(), 1_000 + DEPARTED_ADDRESS_TTL_MS - 1).expect("in window");
        assert_eq!(entry.id, "w0");
        assert_eq!(entry.health, WorkerHealth::Dead);
        assert_eq!(entry.address, addr());
        assert!(
            tombs.lookup(&addr(), 1_000 + DEPARTED_ADDRESS_TTL_MS).is_none(),
            "the window closes at Timer H"
        );
    }

    #[test]
    fn clear_forgets_the_address_at_once() {
        let tombs = Tombstones::default();
        tombs.insert(addr(), "w0".to_string(), 0);
        tombs.clear(&addr());
        assert!(tombs.lookup(&addr(), 0).is_none());
    }

    #[test]
    fn prune_drops_only_closed_windows() {
        let tombs = Tombstones::default();
        tombs.insert(addr(), "w0".to_string(), 0);
        tombs.insert(ProxyAddr::new("10.0.0.2", 5060), "w1".to_string(), 10_000);
        tombs.prune(DEPARTED_ADDRESS_TTL_MS + 1);
        assert!(tombs.lookup(&addr(), DEPARTED_ADDRESS_TTL_MS + 1).is_none());
        assert!(tombs
            .lookup(&ProxyAddr::new("10.0.0.2", 5060), DEPARTED_ADDRESS_TTL_MS + 1)
            .is_some());
    }
}
