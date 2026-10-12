//! [`WatchHealth`] — whether the EndpointSlice watch has a kube client and
//! whether it is delivering, and the episode log of each: one warning naming
//! the cause when a failure starts, one line when it ends.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The watch's own health, for a consumer's metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchHealth {
    /// A kube client is built; until then nothing is watched.
    pub client_built: bool,
    /// Unix time (ms) the watch broke, `None` while it delivers. The last
    /// listed members stay meanwhile.
    pub broken_since_unix_ms: Option<u64>,
}

/// Written by the informer task, read through [`K8sMembership::watch_health`](super::K8sMembership::watch_health).
#[derive(Debug, Default)]
pub(super) struct WatchHealthState {
    client_built: AtomicBool,
    /// 0 while the watch delivers.
    broken_since_unix_ms: AtomicU64,
}

impl WatchHealthState {
    pub(super) fn read(&self) -> WatchHealth {
        let since = self.broken_since_unix_ms.load(Ordering::Relaxed);
        WatchHealth {
            client_built: self.client_built.load(Ordering::Relaxed),
            broken_since_unix_ms: (since != 0).then_some(since),
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_millis().max(1) as u64)
}

/// The episode of building the kube client.
#[derive(Debug, Default)]
pub(super) struct ConnectEpisode {
    failures: u32,
}

impl ConnectEpisode {
    /// One build failed; the next attempt is `retry_in` away.
    pub(super) fn failed(&mut self, cause: &str, retry_in: Duration) {
        self.failures += 1;
        let retry_in_ms = retry_in.as_millis() as u64;
        if self.failures == 1 {
            tracing::warn!(
                cause,
                retry_in_ms,
                "no kube client for the EndpointSlice watch: nothing is discovered until one builds"
            );
        } else {
            tracing::debug!(
                cause,
                retry_in_ms,
                failures = self.failures,
                "kube client still failing"
            );
        }
    }

    /// The client is built.
    pub(super) fn built(&self, state: &WatchHealthState) {
        state.client_built.store(true, Ordering::Relaxed);
        if self.failures > 0 {
            tracing::info!(
                failures = self.failures,
                "kube client built for the EndpointSlice watch"
            );
        }
    }
}

/// The episode of a watch that stopped delivering.
#[derive(Debug, Default)]
pub(super) struct WatchEpisode {
    started: Option<tokio::time::Instant>,
    errors: u64,
}

impl WatchEpisode {
    /// The watch reported an error; it is retried with backoff.
    pub(super) fn error(&mut self, state: &WatchHealthState, cause: &str) {
        self.errors += 1;
        if self.started.is_none() {
            self.started = Some(tokio::time::Instant::now());
            state.broken_since_unix_ms.store(unix_ms(), Ordering::Relaxed);
            tracing::warn!(
                cause,
                "EndpointSlice watch broken: the last listed members stay (will retry)"
            );
        } else {
            tracing::debug!(cause, errors = self.errors, "EndpointSlice watch still broken");
        }
    }

    /// The watch delivered an event.
    pub(super) fn delivered(&mut self, state: &WatchHealthState) {
        let Some(started) = self.started.take() else { return };
        state.broken_since_unix_ms.store(0, Ordering::Relaxed);
        tracing::info!(
            broken_for_ms = started.elapsed().as_millis() as u64,
            errors = self.errors,
            "EndpointSlice watch delivering again"
        );
        self.errors = 0;
    }
}
