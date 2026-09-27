//! [`RefreshBacklog`] — the refreshes an open limiter breaker held back.
//!
//! While the worker's breaker is open no refresh is sent; each one is held
//! here instead, one entry per limiter key, the latest ids winning. The
//! breaker's close takes them all and sends them, so a counted call whose set
//! lapsed on the limiter during the outage is re-registered at the close,
//! not one refresh period later.
//!
//! An entry lives one lease from the first refresh held for its key: a later
//! hold replaces its ids and keeps its place and its expiry. Past it, the
//! call's own refresh, which keeps firing every period, re-registers the set
//! (ADR-0038 decision 4). A hold onto a full backlog gives up the oldest
//! entry, and the release of a call forgets its entry (an ended call's set is
//! never refreshed); all three are counted. An entry is a key and its ids,
//! nothing else of the call. The backlog is not
//! replicated: a worker that dies loses it, and the calls a peer takes over
//! re-register on their own refresh.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::B2buaConfig;
use crate::limiter_release::MAX_LEASE;
use crate::metrics::B2buaMetrics;

/// The backlog's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshBacklogConfig {
    /// How long an entry is worth sending after its first hold: the
    /// limiter's lease.
    pub lease: Duration,
    /// Most entries the backlog holds.
    pub cap: usize,
}

impl RefreshBacklogConfig {
    /// The bounds `config` states: the limiter's lease clamped to
    /// [`MAX_LEASE`], and the release queue's cap.
    pub fn from_config(config: &B2buaConfig) -> Self {
        Self {
            lease: Duration::from_secs(config.limiter_lease_sec.max(0) as u64).min(MAX_LEASE),
            cap: config.limiter_release_queue_cap.max(1),
        }
    }
}

struct Entry {
    ids: Vec<String>,
    /// The entry's place in `order`.
    seq: u64,
    expires_at: Instant,
}

#[derive(Default)]
struct Held {
    by_key: HashMap<String, Entry>,
    /// Keys by first hold: the first expires first.
    order: BTreeMap<u64, String>,
    next_seq: u64,
}

/// The refreshes held while the breaker is open. See the module doc.
pub struct RefreshBacklog {
    config: RefreshBacklogConfig,
    metrics: B2buaMetrics,
    held: Mutex<Held>,
}

impl RefreshBacklog {
    /// An empty backlog under `config`.
    pub fn new(config: RefreshBacklogConfig, metrics: B2buaMetrics) -> Self {
        Self { config, metrics, held: Mutex::new(Held::default()) }
    }

    /// Every step leaves the state whole, so a poisoned lock is taken as it
    /// is.
    fn lock(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hold the refresh of `key` with `ids`: a key already held takes the
    /// new ids and keeps its place and expiry.
    pub fn hold(&self, key: &str, ids: &[String]) {
        let now = Instant::now();
        let mut held = self.lock();
        self.expire(&mut held, now);
        if let Some(entry) = held.by_key.get_mut(key) {
            entry.ids = ids.to_vec();
            return;
        }
        while held.by_key.len() >= self.config.cap {
            let Some((_, oldest)) = held.order.pop_first() else { break };
            held.by_key.remove(&oldest);
            self.metrics.bump_limiter_breaker_refreshes_dropped_cap();
        }
        let seq = held.next_seq;
        held.next_seq += 1;
        held.order.insert(seq, key.to_string());
        let expires_at = now + self.config.lease;
        held.by_key.insert(key.to_string(), Entry { ids: ids.to_vec(), seq, expires_at });
        self.metrics.set_limiter_breaker_refreshes_held(held.by_key.len() as u64);
    }

    /// Every refresh held and not given up, oldest first; the backlog is
    /// left empty.
    pub fn take(&self) -> Vec<(String, Vec<String>)> {
        let mut held = self.lock();
        self.expire(&mut held, Instant::now());
        let order = std::mem::take(&mut held.order);
        let mut by_key = std::mem::take(&mut held.by_key);
        self.metrics.set_limiter_breaker_refreshes_held(0);
        order.into_values().filter_map(|key| by_key.remove(&key).map(|e| (key, e.ids))).collect()
    }

    /// Forget the refresh held for `key`: its call's release was queued.
    pub fn forget(&self, key: &str) {
        let mut held = self.lock();
        if let Some(entry) = held.by_key.remove(key) {
            held.order.remove(&entry.seq);
            self.metrics.bump_limiter_breaker_refreshes_dropped_released();
            self.metrics.set_limiter_breaker_refreshes_held(held.by_key.len() as u64);
        }
    }

    /// Refreshes held.
    pub fn len(&self) -> usize {
        self.lock().by_key.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Give up every entry held for one lease.
    fn expire(&self, held: &mut Held, now: Instant) {
        while let Some(entry) = held.order.first_entry() {
            let key = entry.get();
            if held.by_key.get(key).is_some_and(|e| e.expires_at > now) {
                break;
            }
            let key = entry.remove();
            held.by_key.remove(&key);
            self.metrics.bump_limiter_breaker_refreshes_dropped_lease_expired();
        }
        self.metrics.set_limiter_breaker_refreshes_held(held.by_key.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backlog(cap: usize) -> (RefreshBacklog, B2buaMetrics) {
        let metrics = B2buaMetrics::new();
        let config = RefreshBacklogConfig { lease: Duration::from_secs(20), cap };
        (RefreshBacklog::new(config, metrics.clone()), metrics)
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn one_entry_per_key_and_the_latest_ids_win() {
        let (b, metrics) = backlog(10);
        b.hold("a", &ids(&["x"]));
        b.hold("b", &ids(&["y"]));
        b.hold("a", &ids(&["x", "z"]));
        assert_eq!(metrics.limiter_breaker_refreshes_held(), 2);
        assert_eq!(b.take(), [("a".to_string(), ids(&["x", "z"])), ("b".to_string(), ids(&["y"]))]);
        assert!(b.is_empty());
        assert_eq!(metrics.limiter_breaker_refreshes_held(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_lives_one_lease_from_its_first_hold() {
        let (b, metrics) = backlog(10);
        b.hold("first", &ids(&["x"]));
        tokio::time::advance(Duration::from_secs(5)).await;
        b.hold("later", &ids(&["x"]));
        tokio::time::advance(Duration::from_secs(10)).await;
        b.hold("first", &ids(&["x", "y"]));
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(b.take(), [("later".to_string(), ids(&["x"]))], "a re-hold renews nothing");
        assert_eq!(metrics.limiter_breaker_refreshes_dropped_lease_expired_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_released_call_s_entry_is_forgotten() {
        let (b, metrics) = backlog(10);
        b.hold("ended", &ids(&["x"]));
        b.hold("live", &ids(&["x"]));
        b.forget("ended");
        b.forget("never-held");
        assert_eq!(b.take(), [("live".to_string(), ids(&["x"]))]);
        assert_eq!(metrics.limiter_breaker_refreshes_dropped_released_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_backlog_gives_up_its_oldest_entry() {
        let (b, metrics) = backlog(2);
        b.hold("a", &ids(&["x"]));
        b.hold("b", &ids(&["x"]));
        b.hold("c", &ids(&["x"]));
        assert_eq!(b.len(), 2);
        assert_eq!(metrics.limiter_breaker_refreshes_dropped_cap_total(), 1);
        let keys: Vec<String> = b.take().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["b", "c"]);
    }
}
