//! [`WatchHealth`] — whether the EndpointSlice watch has a kube client and
//! whether it is delivering, and the episode log of each: one warning naming
//! the cause when a failure starts, one line when it ends.
//!
//! A watch that restarts on a still valid resource version delivers no event
//! until the next change, so a broken watch also ends once it has gone
//! [`WATCH_QUIET_PERIOD`] without an error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A broken watch with no error for this long is delivering again: longer
/// than the longest gap between two errors of a broken watch, the watcher's
/// backoff ceiling (30 s) plus the client connect timeout (30 s), with margin.
pub const WATCH_QUIET_PERIOD: Duration = Duration::from_secs(90);

/// The watch's own health, for a consumer's metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchHealth {
    /// A kube client is built; until then nothing is watched.
    pub client_built: bool,
    /// A kube client failed to build and none is built yet. False before the
    /// first attempt has ended.
    pub client_failing: bool,
    /// Unix time (ms) the watch broke, `None` while it delivers. The last
    /// listed members stay meanwhile.
    pub broken_since_unix_ms: Option<u64>,
    /// The informer task ended: nothing watches any more, and the last
    /// listed members stay.
    pub ended: bool,
}

/// Written by the informer task, read through [`K8sMembership::watch_health`](super::K8sMembership::watch_health).
#[derive(Debug, Default)]
pub(super) struct WatchHealthState {
    client_built: AtomicBool,
    client_failed: AtomicBool,
    /// 0 while the watch delivers.
    broken_since_unix_ms: AtomicU64,
}

impl WatchHealthState {
    /// The health as the task wrote it, its task `ended` or not.
    pub(super) fn read(&self, ended: bool) -> WatchHealth {
        let since = self.broken_since_unix_ms.load(Ordering::Relaxed);
        let client_built = self.client_built.load(Ordering::Relaxed);
        WatchHealth {
            client_built,
            client_failing: !client_built && self.client_failed.load(Ordering::Relaxed),
            broken_since_unix_ms: (since != 0).then_some(since),
            ended,
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| millis(d).max(1))
}

/// `d` in whole milliseconds, held at `u64::MAX`.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The episode of building the kube client.
#[derive(Debug, Default)]
pub(super) struct ConnectEpisode {
    failures: u32,
}

impl ConnectEpisode {
    /// One build failed; the next attempt is `retry_in` away.
    pub(super) fn failed(&mut self, state: &WatchHealthState, cause: &str, retry_in: Duration) {
        self.failures += 1;
        state.client_failed.store(true, Ordering::Relaxed);
        let retry_in_ms = millis(retry_in);
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
    last_error: Option<tokio::time::Instant>,
    errors: u64,
}

impl WatchEpisode {
    /// The watch reported an error; it is retried with backoff.
    pub(super) fn error(&mut self, state: &WatchHealthState, cause: &str) {
        self.errors += 1;
        self.last_error = Some(tokio::time::Instant::now());
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

    /// When the open episode ends unless another error comes first: the
    /// last error plus [`WATCH_QUIET_PERIOD`]. `None` with no open episode.
    pub(super) fn quiet_deadline(&self) -> Option<tokio::time::Instant> {
        self.started?;
        self.last_error.map(|at| at + WATCH_QUIET_PERIOD)
    }

    /// The watch delivered an event, or went the quiet period without an
    /// error: it is delivering again.
    pub(super) fn delivered(&mut self, state: &WatchHealthState) {
        let Some(started) = self.started.take() else { return };
        self.last_error = None;
        state.broken_since_unix_ms.store(0, Ordering::Relaxed);
        tracing::info!(
            broken_for_ms = millis(started.elapsed()),
            errors = self.errors,
            "EndpointSlice watch delivering again"
        );
        self.errors = 0;
    }
}
