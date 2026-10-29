//! The run as its report states it: the load run index assembled from the
//! reporter, the mux, the rate handle and the call-outcome sink, the live
//! report the metrics socket serves from it, and the status line printed on
//! each window of the time series.

use std::sync::Arc;

use sip_clock::Clock;

use crate::outcome::OutcomeSink;
use crate::{
    Canaries, LiveReport, LoadRunIndex, LoadRunMeta, MuxCore, Progress, RateHandle, Reporter,
};

/// Every source of one run's index.
pub(super) struct RunView {
    pub(super) reporter: Arc<Reporter>,
    pub(super) core: Arc<MuxCore>,
    pub(super) rate: RateHandle,
    pub(super) outcomes: Arc<OutcomeSink>,
    pub(super) clock: Clock,
    /// The run's settings; each index stamps its own time and `finished`.
    pub(super) meta: LoadRunMeta,
}

impl RunView {
    /// The run's index as of now.
    pub(super) fn index(&self, finished: bool) -> LoadRunIndex {
        let meta = LoadRunMeta { finished_ms: self.clock.now_ms(), finished, ..self.meta.clone() };
        let mut idx = self.reporter.build_index(meta, mux_canaries(&self.core));
        self.complete(&mut idx.progress);
        let (events, dropped) = self.rate.events_since(self.reporter.started_at());
        idx.rate_events = events;
        idx.rate_events_dropped = dropped;
        idx
    }

    /// The headline figures as of now.
    pub(super) fn progress(&self) -> Progress {
        let mut progress = self.reporter.progress();
        self.complete(&mut progress);
        progress
    }

    /// Fill the figures the reporter does not hold: the rate and the
    /// call-outcome counts.
    fn complete(&self, p: &mut Progress) {
        p.target_cps = self.rate.cps();
        p.draining = self.rate.is_draining();
        p.offered = self.outcomes.offered_total();
        p.not_placed = self.outcomes.not_placed_total();
        p.generator_failures = self.outcomes.generator_failures_total();
    }

    /// The live report of this run, for the metrics socket.
    pub(super) fn live_report(self: &Arc<Self>) -> LiveReport {
        let (view, pages) = (Arc::clone(self), Arc::clone(&self.reporter));
        LiveReport {
            index: Arc::new(move || view.index(false)),
            page: Arc::new(move |rel| pages.sample_page(rel)),
        }
    }

    /// Spawn the time-series task: every [`crate::SERIES_TICK`] it closes a
    /// window and prints the run's status line on stderr. Abort it before the
    /// final window is closed.
    pub(super) fn spawn_ticker(
        self: &Arc<Self>,
        name: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let view = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::SERIES_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let window = view.reporter.tick();
                let line =
                    loadrun_render::status_line(&view.progress(), view.meta.duration_secs, &window);
                eprintln!("[{name}] {line}");
            }
        })
    }
}

/// The mux-owned run canaries (orphans, simulated-loss drops each way and full
/// inbox drops). The reporter fills in its own shed and ringing halves.
pub(super) fn mux_canaries(core: &MuxCore) -> Canaries {
    use std::sync::atomic::Ordering;
    let s = core.stats();
    Canaries {
        orphans: s.orphans_total(),
        drops_in: s.dropped_in.load(Ordering::Relaxed),
        drops_out: s.dropped_out.load(Ordering::Relaxed),
        inbox_drops: s.inbox_drop.load(Ordering::Relaxed),
        ..Canaries::default()
    }
}
