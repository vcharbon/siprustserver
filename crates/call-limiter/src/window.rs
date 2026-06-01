//! [`WindowStore`] — the windowed counter core.
//!
//! Port of `CallLimiter.memory.ts` with the per-op atomics from the Redis Lua
//! scripts (`CallLimiter.redis.ts`):
//!
//! - **admit** (`CHECK_AND_INCREMENT_LUA`, here **batched/transactional**):
//!   under one lock, for every entry sum its last `active_windows` window
//!   counts; if **any** total `>= limit`, reject and increment nothing; else
//!   `INCR current` (+ refresh TTL) for **every** entry and return the shared
//!   current window.
//! - **release** (`DECREMENT_LUA`, floored): `count = max(0, count - 1)`.
//!   The Lua `DECR` could go negative; we take the memory-impl flooring since we
//!   own the store.
//! - **refresh** (`REFRESH_LUA`): if `origin == current`, noop; else
//!   `INCR current` (+TTL) **then** `DECR origin` — incr-before-decr briefly
//!   overcounts, never undercounts.
//! - **sweep**: per-key `expires_at_ms = now + ttl`; the whole store is swept on
//!   every access (and by the runner's janitor via [`WindowStore::sweep_now`]).
//!
//! All time is read through the injected [`Clock`], so windows + TTL advance
//! deterministically under a paused test clock.

use std::collections::HashMap;
use std::sync::Mutex;

use sip_clock::Clock;

use crate::wire::{AdmitEntry, Hold};

/// Sliding-window configuration. Defaults match the TS `AppConfig`.
#[derive(Clone, Copy, Debug)]
pub struct LimiterConfig {
    /// Seconds per window.
    pub window_sec: i64,
    /// Number of windows summed in the lookback.
    pub active_windows: i64,
    /// Per-key TTL in seconds (auto-clear if refresh stops; crash recovery).
    pub ttl_sec: i64,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            window_sec: 300,
            active_windows: 3,
            ttl_sec: 1200,
        }
    }
}

impl LimiterConfig {
    /// Round an epoch-second down to the window boundary.
    fn window_of(&self, epoch_sec: i64) -> i64 {
        epoch_sec - epoch_sec.rem_euclid(self.window_sec)
    }
}

/// The transactional admit outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitResult {
    /// Every entry was incremented at this window.
    Admitted {
        /// The shared current window all entries landed in.
        window: i64,
    },
    /// At least one entry was over cap; nothing was incremented.
    Rejected {
        /// The first id found over cap.
        limiter_id: String,
    },
}

#[derive(Clone, Copy)]
struct Entry {
    count: i64,
    expires_at_ms: i64,
}

struct Inner {
    /// `(limiter_id, window)` -> counter.
    map: HashMap<(String, i64), Entry>,
    /// Cumulative count of keys removed by TTL sweep (the `auto_cleared` metric).
    auto_cleared: u64,
}

/// The windowed counter store. Interior-mutable so it can be shared (`Arc`) by
/// the HTTP handler and the janitor.
pub struct WindowStore {
    inner: Mutex<Inner>,
    clock: Clock,
    cfg: LimiterConfig,
}

impl WindowStore {
    /// Build a store over the injected clock.
    pub fn new(cfg: LimiterConfig, clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                auto_cleared: 0,
            }),
            clock,
            cfg,
        }
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// The current window timestamp (server-authoritative).
    pub fn current_window(&self) -> i64 {
        self.cfg.window_of(self.now_ms() / 1000)
    }

    /// Transactional admit: increment every entry's current window, or none.
    pub fn admit(&self, entries: &[AdmitEntry]) -> AdmitResult {
        let now_ms = self.now_ms();
        let cur = self.cfg.window_of(now_ms / 1000);
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);

        // Phase 1: check every entry against the sum of its last N windows.
        for e in entries {
            let mut total = 0;
            for i in 0..self.cfg.active_windows {
                let w = cur - (self.cfg.active_windows - 1 - i) * self.cfg.window_sec;
                if let Some(en) = inner.map.get(&(e.id.clone(), w)) {
                    total += en.count;
                }
            }
            if total >= e.limit {
                return AdmitResult::Rejected {
                    limiter_id: e.id.clone(),
                };
            }
        }

        // Phase 2: all clear — increment every entry's current window.
        let expires = now_ms + self.cfg.ttl_sec * 1000;
        for e in entries {
            let en = inner
                .map
                .entry((e.id.clone(), cur))
                .or_insert(Entry {
                    count: 0,
                    expires_at_ms: expires,
                });
            en.count += 1;
            en.expires_at_ms = expires;
        }
        AdmitResult::Admitted { window: cur }
    }

    /// Release each hold: decrement its window's counter, floored at 0.
    pub fn release(&self, holds: &[Hold]) {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        for h in holds {
            if let Some(en) = inner.map.get_mut(&(h.id.clone(), h.window)) {
                en.count = (en.count - 1).max(0);
            }
        }
    }

    /// Refresh each hold: migrate it from its origin window to the current one
    /// (incr-current-before-decr-origin). Returns the holds with new windows.
    pub fn refresh(&self, holds: &[Hold]) -> Vec<Hold> {
        let now_ms = self.now_ms();
        let cur = self.cfg.window_of(now_ms / 1000);
        let mut inner = self.inner.lock().unwrap();
        sweep(&mut inner, now_ms);
        let expires = now_ms + self.cfg.ttl_sec * 1000;

        let mut out = Vec::with_capacity(holds.len());
        for h in holds {
            if h.window != cur {
                // INCR current first (briefly overcounts, never undercounts).
                let en = inner.map.entry((h.id.clone(), cur)).or_insert(Entry {
                    count: 0,
                    expires_at_ms: expires,
                });
                en.count += 1;
                en.expires_at_ms = expires;
                // DECR origin (floored).
                if let Some(o) = inner.map.get_mut(&(h.id.clone(), h.window)) {
                    o.count = (o.count - 1).max(0);
                }
            }
            out.push(Hold {
                id: h.id.clone(),
                window: cur,
            });
        }
        out
    }

    /// Sweep TTL-expired keys now (the janitor entry point). Returns how many
    /// keys were removed.
    pub fn sweep_now(&self) -> u64 {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock().unwrap();
        let before = inner.auto_cleared;
        sweep(&mut inner, now_ms);
        inner.auto_cleared - before
    }

    /// Live gauges + the cumulative auto-clear counter, for metrics.
    pub fn stats(&self) -> WindowStats {
        let inner = self.inner.lock().unwrap();
        WindowStats {
            live_keys: inner.map.len() as u64,
            current_total: inner.map.values().map(|e| e.count).sum(),
            auto_cleared: inner.auto_cleared,
        }
    }
}

/// A snapshot of the store's metric-relevant numbers.
#[derive(Clone, Copy, Debug)]
pub struct WindowStats {
    /// Number of live `(id, window)` keys (leak monitor).
    pub live_keys: u64,
    /// Sum of all live counts (current concurrent across all ids).
    pub current_total: i64,
    /// Cumulative keys removed by TTL sweep.
    pub auto_cleared: u64,
}

/// Drop every key whose TTL has elapsed; bump `auto_cleared`.
fn sweep(inner: &mut Inner, now_ms: i64) {
    let before = inner.map.len();
    inner.map.retain(|_, e| e.expires_at_ms > now_ms);
    inner.auto_cleared += (before - inner.map.len()) as u64;
}
