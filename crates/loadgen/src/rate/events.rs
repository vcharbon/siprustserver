//! The rate handle's history: every rate change and the drain, each stamped
//! when it happened, bounded.

use e2e_model::{RateEvent, RateEventKind};
use tokio::time::Instant;

/// The most events the log holds; later ones are only counted.
pub(crate) const RATE_EVENTS_CAP: usize = 256;

/// The bounded event log of one rate handle.
#[derive(Debug, Default)]
pub(crate) struct RateLog {
    entries: Vec<(Instant, RateEventKind, f64, f64)>,
    dropped: u64,
}

impl RateLog {
    pub(crate) fn push(&mut self, kind: RateEventKind, from_cps: f64, to_cps: f64) {
        if self.entries.len() < RATE_EVENTS_CAP {
            self.entries.push((Instant::now(), kind, from_cps, to_cps));
        } else {
            self.dropped += 1;
        }
    }

    /// The events, each stamped in milliseconds since `start`, and the count of
    /// those past the bound.
    pub(crate) fn since(&self, start: Instant) -> (Vec<RateEvent>, u64) {
        let events = self
            .entries
            .iter()
            .map(|(at, kind, from_cps, to_cps)| RateEvent {
                at_ms: at.saturating_duration_since(start).as_millis() as u64,
                kind: *kind,
                from_cps: *from_cps,
                to_cps: *to_cps,
            })
            .collect();
        (events, self.dropped)
    }
}
