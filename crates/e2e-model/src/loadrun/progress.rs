//! The run's progress as a load run index states it: the headline figures, the
//! time series, the rate events and the last failures. Every time is in
//! milliseconds since the run started.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The run's headline figures as of the doc.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// Time since the run started.
    pub elapsed_ms: u64,
    /// Time the run offered calls so far: the elapsed time up to the run's
    /// duration or its drain, whichever came first.
    pub offering_ms: u64,
    /// The offered-rate target (calls/s) as of the doc: the configured rate
    /// until a rate change moves it, `0` while paused.
    pub target_cps: f64,
    /// The run was drained: it places no new call.
    pub draining: bool,
    /// Calls offered: every slot of the rate, placed or not.
    pub offered: u64,
    /// Calls placed: started past the max-in-flight cap.
    pub placed: u64,
    /// Calls placed and not ended yet.
    pub in_flight: u64,
    /// Calls ended on an accepted branch.
    pub ok: u64,
    /// Calls ended in failure: every class but `ok` and `rejected`.
    pub failed: u64,
    /// Calls of the `rejected` class: refused before any datagram.
    pub rejected: u64,
    /// Calls the generator itself failed: a run with one is invalid.
    pub generator_failures: u64,
    /// Datagram copies received after the first, over every call.
    pub retransmissions_received: u64,
    /// Datagram copies sent after the first, over every call.
    pub retransmissions_resent: u64,
    /// Some call ran the retransmit engine (re-sends are counted only then).
    pub retransmit_engine: bool,
}

impl Progress {
    /// Calls placed per second of the time the run offered calls (`0` before
    /// any).
    pub fn achieved_cps(&self) -> f64 {
        if self.offering_ms == 0 {
            0.0
        } else {
            self.placed as f64 * 1000.0 / self.offering_ms as f64
        }
    }
}

/// One window of the run's time series: what changed between its start and
/// its end.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPoint {
    /// Window start, since the run started.
    pub start_ms: u64,
    /// Window length.
    pub span_ms: u64,
    /// Calls placed in the window.
    pub placed: u64,
    /// Calls ended ok in the window.
    pub ok: u64,
    /// Calls ended in failure in the window (as [`Progress::failed`]).
    pub failed: u64,
    /// Calls refused before any datagram or shed at the cap in the window.
    pub rejected: u64,
    /// Retransmissions received by the calls ended in the window.
    pub retransmissions_received: u64,
    /// Re-sends by the calls ended in the window.
    pub retransmissions_resent: u64,
    /// End-to-end latency median of the calls ended in the window (`0` when
    /// none, see [`Self::has_latency`]).
    pub p50_ms: f64,
    /// End-to-end latency 99th percentile of the calls ended in the window.
    pub p99_ms: f64,
}

impl SeriesPoint {
    /// The failed share of the calls that ran and ended in the window, in
    /// percent; `None` when none ended.
    pub fn error_pct(&self) -> Option<f64> {
        let ended = self.ok + self.failed;
        (ended > 0).then(|| self.failed as f64 * 100.0 / ended as f64)
    }

    /// Window end, since the run started.
    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.span_ms
    }

    /// Whether `at_ms` falls in the window.
    pub fn holds(&self, at_ms: u64) -> bool {
        (self.start_ms..self.end_ms()).contains(&at_ms)
    }

    /// Retransmissions received and re-sent per call that ran and ended in the
    /// window; `None` when none ended.
    pub fn retransmissions_per_call(&self) -> Option<f64> {
        let ended = self.ok + self.failed;
        let retr = self.retransmissions_received + self.retransmissions_resent;
        (ended > 0).then(|| retr as f64 / ended as f64)
    }

    /// Whether the window holds a call's end-to-end latency: some call that
    /// ran ended in it.
    pub fn has_latency(&self) -> bool {
        self.ok + self.failed > 0
    }
}

/// What a rate event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RateEventKind {
    /// The offered-rate target moved from `fromCps` to `toCps`.
    Rate,
    /// The run was drained: no new call from then on.
    Drain,
}

/// One change of the offered rate, or the drain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateEvent {
    /// When it happened, since the run started.
    pub at_ms: u64,
    pub kind: RateEventKind,
    /// The target before the event.
    pub from_cps: f64,
    /// The target after the event (equal to `fromCps` for a drain).
    pub to_cps: f64,
}

/// One failed call among the run's last failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureRow {
    /// When the call ended, since the run started.
    pub at_ms: u64,
    pub scenario: String,
    pub class: String,
    /// The class's case discriminator (see [`super::CountRow::case`]).
    #[serde(default)]
    pub case: String,
    /// The run-dir-relative path of the call's stored sample page, when it was
    /// stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
}
