//! The resolver's bounded, two-sided-TTL name cache (see the parent module).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_clock::Clock;

use super::ResolverConfig;
use crate::poison::locked;

/// One cached resolution outcome. `None` = negative entry (lookup failed).
struct CacheEntry {
    outcome: Option<SocketAddr>,
    expires_at_ms: u64,
    /// Set on every cache Hit; the proactive refresh reads-and-clears it to
    /// decide refresh-vs-idle-stop (see the module doc).
    used_since_refresh: bool,
}

pub(super) enum CacheLookup {
    Hit(SocketAddr),
    NegativeHit,
    Miss,
}

/// Bounded, two-sided-TTL name cache. Lock-only (`std::sync::Mutex`, never
/// held across an await).
pub(super) struct NameCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    clock: Clock,
    pub(super) cfg: ResolverConfig,
}

impl NameCache {
    /// An empty cache under `cfg`, timed by `clock`.
    pub(super) fn new(clock: Clock, cfg: ResolverConfig) -> Self {
        Self { entries: Mutex::new(HashMap::new()), clock, cfg }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    pub(super) fn lookup(&self, key: &str) -> CacheLookup {
        let now = self.now_ms();
        let mut entries = locked(&self.entries, "resolver cache");
        match entries.get_mut(key) {
            Some(e) if e.expires_at_ms <= now => {
                entries.remove(key);
                CacheLookup::Miss
            }
            Some(e) => match e.outcome {
                Some(addr) => {
                    e.used_since_refresh = true;
                    CacheLookup::Hit(addr)
                }
                None => CacheLookup::NegativeHit,
            },
            None => CacheLookup::Miss,
        }
    }

    /// Read-and-clear the used-since-refresh flag for the refresh decision.
    /// `None` when there is no live positive entry (missing, expired, or
    /// negative) — the refresh treats that like "unused".
    pub(super) fn take_used(&self, key: &str) -> Option<bool> {
        let now = self.now_ms();
        let mut entries = locked(&self.entries, "resolver cache");
        let e = entries.get_mut(key)?;
        if e.expires_at_ms <= now || e.outcome.is_none() {
            return None;
        }
        let used = e.used_since_refresh;
        e.used_since_refresh = false;
        Some(used)
    }

    /// Store an outcome (positive or negative TTL by kind); returns the new
    /// size for the gauge. At the cap, expired entries are reclaimed first,
    /// then an arbitrary live entry is evicted — new names must always be able
    /// to enter, else a name-flood would pin the cache against real targets.
    pub(super) fn store(&self, key: &str, outcome: Option<SocketAddr>) -> usize {
        let now = self.now_ms();
        let ttl =
            if outcome.is_some() { self.cfg.positive_ttl_ms } else { self.cfg.negative_ttl_ms };
        let mut entries = locked(&self.entries, "resolver cache");
        if entries.len() >= self.cfg.max_entries && !entries.contains_key(key) {
            entries.retain(|_, e| e.expires_at_ms > now);
            if entries.len() >= self.cfg.max_entries {
                if let Some(victim) = entries.keys().next().cloned() {
                    entries.remove(&victim);
                }
            }
        }
        entries.insert(
            key.to_string(),
            CacheEntry { outcome, expires_at_ms: now + ttl, used_since_refresh: false },
        );
        entries.len()
    }
}
