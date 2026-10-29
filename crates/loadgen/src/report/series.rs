//! The run's time series: a ring of at most [`SERIES_CAP`] windows that
//! covers the whole run in fixed memory. Each tick closes one window of
//! deltas; when the ring is full, adjacent windows merge in pairs and every
//! later window spans twice as many ticks.

use std::time::Duration;

use e2e_model::SeriesPoint;

use crate::hist::Hist;

/// How often the run closes a window of its time series.
pub const SERIES_TICK: Duration = Duration::from_secs(5);

/// The most windows the ring holds.
pub(crate) const SERIES_CAP: usize = 720;

/// The counters of the run a window holds the deltas of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub(crate) placed: u64,
    pub(crate) ok: u64,
    pub(crate) failed: u64,
    pub(crate) rejected: u64,
}

impl Totals {
    fn since(self, earlier: Totals) -> Totals {
        Totals {
            placed: self.placed.saturating_sub(earlier.placed),
            ok: self.ok.saturating_sub(earlier.ok),
            failed: self.failed.saturating_sub(earlier.failed),
            rejected: self.rejected.saturating_sub(earlier.rejected),
        }
    }

    fn add(&mut self, other: Totals) {
        self.placed += other.placed;
        self.ok += other.ok;
        self.failed += other.failed;
        self.rejected += other.rejected;
    }
}

/// One window: its span, its counter deltas and the end-to-end latencies of
/// the calls that ended in it.
#[derive(Clone)]
struct Window {
    start_ms: u64,
    end_ms: u64,
    ticks: u32,
    totals: Totals,
    latency: Hist,
}

impl Window {
    fn absorb(&mut self, next: &Window) {
        self.end_ms = next.end_ms;
        self.ticks += next.ticks;
        self.totals.add(next.totals);
        self.latency.merge(&next.latency);
    }

    fn point(&self) -> SeriesPoint {
        SeriesPoint {
            start_ms: self.start_ms,
            span_ms: self.end_ms - self.start_ms,
            placed: self.totals.placed,
            ok: self.totals.ok,
            failed: self.totals.failed,
            rejected: self.totals.rejected,
            p50_ms: self.latency.quantile_ms(0.5),
            p99_ms: self.latency.quantile_ms(0.99),
        }
    }
}

/// The ring, and the cumulative state the next tick takes its deltas from.
pub(crate) struct SeriesRing {
    windows: Vec<Window>,
    /// Ticks one window spans: doubles at each merge.
    ticks_per_window: u32,
    last_ms: u64,
    last_totals: Totals,
    last_latency: Hist,
}

impl SeriesRing {
    pub(crate) fn new() -> Self {
        Self {
            windows: Vec::new(),
            ticks_per_window: 1,
            last_ms: 0,
            last_totals: Totals::default(),
            last_latency: Hist::call(),
        }
    }

    /// Close the tick ending at `now_ms` with the run's cumulative `totals`
    /// and end-to-end `latency`, and return it as a point.
    pub(crate) fn tick(&mut self, now_ms: u64, totals: Totals, latency: &Hist) -> SeriesPoint {
        let window = Window {
            start_ms: self.last_ms,
            end_ms: now_ms.max(self.last_ms),
            ticks: 1,
            totals: totals.since(self.last_totals),
            latency: latency.since(&self.last_latency),
        };
        let point = window.point();
        self.last_ms = window.end_ms;
        self.last_totals = totals;
        self.last_latency = latency.clone();
        match self.windows.last_mut() {
            Some(last) if last.ticks < self.ticks_per_window => last.absorb(&window),
            _ => self.windows.push(window),
        }
        if self.windows.len() > SERIES_CAP {
            self.halve();
        }
        point
    }

    /// Merge adjacent windows in pairs: the ring holds half as many, each
    /// twice as long.
    fn halve(&mut self) {
        let mut merged: Vec<Window> = Vec::with_capacity(SERIES_CAP / 2 + 1);
        for pair in self.windows.chunks(2) {
            let mut w = pair[0].clone();
            if let Some(next) = pair.get(1) {
                w.absorb(next);
            }
            merged.push(w);
        }
        self.windows = merged;
        self.ticks_per_window *= 2;
    }

    /// Every window, oldest first.
    pub(crate) fn points(&self) -> Vec<SeriesPoint> {
        self.windows.iter().map(Window::point).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(n: u64) -> Totals {
        Totals { placed: n, ok: n, failed: 0, rejected: 0 }
    }

    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_full_ring_merges_and_still_covers_the_whole_run() {
        let mut ring = SeriesRing::new();
        let latency = Hist::call();
        let ticks = 5 * SERIES_CAP as u64 + 3;
        for i in 1..=ticks {
            ring.tick(i * 5_000, totals(i * 2), &latency);
            assert!(ring.windows.len() <= SERIES_CAP, "bounded at tick {i}");
        }
        let points = ring.points();
        assert_eq!(points.first().map(|p| p.start_ms), Some(0), "the run's start is covered");
        assert_eq!(points.last().map(SeriesPoint::end_ms), Some(ticks * 5_000), "up to now");
        for pair in points.windows(2) {
            assert_eq!(pair[0].end_ms(), pair[1].start_ms, "no gap between windows");
        }
        assert_eq!(points.iter().map(|p| p.placed).sum::<u64>(), ticks * 2, "every call counted");
        assert!(points.len() > SERIES_CAP / 2, "merging halves, never more: {}", points.len());
        assert_eq!(points[0].span_ms, 8 * 5_000, "three merges: 8 ticks per window");
    }

    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_merged_window_keeps_the_latency_of_both_halves() {
        let mut ring = SeriesRing::new();
        let mut latency = Hist::call();
        ring.tick(5_000, totals(0), &latency);
        for _ in 0..100 {
            latency.record(500.0);
        }
        ring.tick(10_000, totals(100), &latency);
        for i in 3..=SERIES_CAP as u64 + 1 {
            ring.tick(i * 5_000, totals(100), &latency);
        }
        let first = &ring.points()[0];
        assert_eq!(
            (first.start_ms, first.span_ms, first.ok),
            (0, 10_000, 100),
            "merged: {first:?}"
        );
        assert!(first.p50_ms > 400.0, "the second half's calls kept: {first:?}");
    }

    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_window_holds_the_latency_of_the_calls_that_ended_in_it() {
        let mut ring = SeriesRing::new();
        let mut latency = Hist::call();
        for _ in 0..100 {
            latency.record(1.0);
        }
        ring.tick(5_000, totals(100), &latency);
        for _ in 0..100 {
            latency.record(500.0);
        }
        let point = ring.tick(10_000, totals(200), &latency);
        assert!(point.p50_ms > 400.0, "only the second window's calls: {point:?}");
        assert_eq!((point.start_ms, point.span_ms, point.ok), (5_000, 5_000, 100));
    }
}
