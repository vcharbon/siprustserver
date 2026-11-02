//! The health probe's cadence and when it reads a silent worker `Dead`.

use std::time::Duration;

/// Probe cadence (source defaults: 1 s interval, 1.5 s reply window, 2 misses
/// → dead). `interval_ms` is the tick period; `timeout_ms` is each probe's own
/// reply window (windows may overlap ticks — probes are branch-correlated).
#[derive(Debug, Clone, Copy)]
pub struct HealthProbeConfig {
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub threshold: u32,
}

impl Default for HealthProbeConfig {
    fn default() -> Self {
        Self { interval_ms: 1_000, timeout_ms: 1_500, threshold: 2 }
    }
}

impl HealthProbeConfig {
    /// The tick period the probe runs at: `interval_ms`, at least 1 ms.
    pub fn period(&self) -> Duration {
        Duration::from_millis(self.interval_ms.max(1))
    }

    /// From the send of the first probe a worker leaves unanswered to its
    /// `Dead` verdict: `(threshold − 1) × interval + timeout`. Each probe is
    /// reaped at the end of its own reply window, and the `threshold`-th
    /// consecutive miss is the verdict.
    pub fn verdict_after_first_miss(&self) -> Duration {
        let misses_before_last = self.threshold.saturating_sub(1);
        self.period()
            .saturating_mul(misses_before_last)
            .saturating_add(Duration::from_millis(self.timeout_ms))
    }

    /// The latest `Dead` verdict after a worker falls silent:
    /// `threshold × interval + timeout`. The first probe it leaves unanswered
    /// leaves within one interval of its silence (at once when the silence
    /// swallows a probe already sent, one interval later when the worker
    /// answered the probe of the tick just before).
    pub fn verdict_within(&self) -> Duration {
        self.period().saturating_add(self.verdict_after_first_miss())
    }
}
