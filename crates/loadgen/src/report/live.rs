//! What the reporter holds for the live view of a run: the time series, the
//! run-wide end-to-end latency it windows, and the last failed calls.

use std::collections::VecDeque;

use e2e_model::{FailureRow, Progress, SeriesPoint};
use tokio::time::Instant;

use super::series::{SeriesRing, Totals};
use super::{Bucket, Inner, Reporter};
use crate::class::ResultClass;
use crate::hist::Hist;

/// The most failed calls the reporter keeps.
pub(crate) const RECENT_FAILURES: usize = 20;

pub(super) struct Live {
    pub(super) series: SeriesRing,
    /// Every call's end-to-end time, all scenarios merged.
    pub(super) latency: Hist,
    /// The last failed calls, oldest first.
    pub(super) failures: VecDeque<FailureRow>,
}

impl Default for Live {
    fn default() -> Self {
        Self { series: SeriesRing::new(), latency: Hist::call(), failures: VecDeque::new() }
    }
}

impl Live {
    /// Keep a failed call of `bucket` that ended at `at_ms`, with its stored
    /// sample page if any, dropping the oldest past the bound.
    pub(super) fn note_failure(&mut self, at_ms: u64, bucket: &Bucket, page: Option<String>) {
        let (scenario, class, case) = bucket;
        if self.failures.len() == RECENT_FAILURES {
            self.failures.pop_front();
        }
        self.failures.push_back(FailureRow {
            at_ms,
            scenario: scenario.to_string(),
            class: class.clone(),
            case: case.clone(),
            page,
        });
    }
}

/// The calls ended ok, ended in failure and rejected, from the counts.
fn ended(g: &Inner) -> (u64, u64, u64) {
    let (ok, rejected) = (ResultClass::Ok.label(), ResultClass::Rejected.label());
    g.counts.iter().fold((0, 0, 0), |(o, f, r), ((_, class, _), n)| {
        if *class == ok {
            (o + n, f, r)
        } else if *class == rejected {
            (o, f, r + n)
        } else {
            (o, f + n, r)
        }
    })
}

/// The retransmissions received and re-sent over every call folded so far.
fn retransmissions(g: &Inner) -> (u64, u64) {
    g.ladders.values().fold((0, 0), |(r, s), l| (r + l.received, s + l.resent))
}

/// The requests answered again after their call ended, so far.
fn late_answered(g: &Inner) -> u64 {
    g.ladders.values().map(|l| l.late).sum()
}

impl Reporter {
    /// Milliseconds from the run's start to `at`.
    pub fn ms_since_start(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_millis() as u64
    }

    /// The run's start, the origin of every time the reporter states.
    pub fn started_at(&self) -> Instant {
        self.start
    }

    /// The headline figures the reporter holds: elapsed time, calls placed,
    /// in flight, ended ok, failed and rejected. The rate and the call-outcome
    /// counts are the caller's.
    pub fn progress(&self) -> Progress {
        let g = self.inner.lock().unwrap();
        self.progress_of(&g)
    }

    pub(super) fn progress_of(&self, g: &Inner) -> Progress {
        let (ok, failed, rejected) = ended(g);
        let (received, resent) = retransmissions(g);
        let elapsed_ms = self.ms_since_start(Instant::now());
        Progress {
            elapsed_ms,
            offering_ms: elapsed_ms,
            placed: self.started.load(std::sync::atomic::Ordering::Relaxed),
            in_flight: self.inflight.load(std::sync::atomic::Ordering::Relaxed),
            ok,
            failed,
            rejected,
            retransmissions_received: received,
            retransmissions_resent: resent,
            retransmit_engine: g.ladders.values().any(|l| l.engine),
            ..Progress::default()
        }
    }

    /// Close the time series' current window at now and return it: the calls
    /// placed, ended ok, failed, and rejected or shed since the last tick, and
    /// the end-to-end latency of the calls ended since.
    pub fn tick(&self) -> SeriesPoint {
        let now_ms = self.ms_since_start(Instant::now());
        let mut g = self.inner.lock().unwrap();
        let (ok, failed, rejected) = ended(&g);
        let totals = Totals {
            placed: self.started.load(std::sync::atomic::Ordering::Relaxed),
            ok,
            failed,
            rejected: rejected + g.shed.values().sum::<u64>(),
            received: retransmissions(&g).0,
            resent: retransmissions(&g).1,
            late: late_answered(&g),
        };
        let Live { series, latency, .. } = &mut g.live;
        series.tick(now_ms, totals, latency)
    }
}
