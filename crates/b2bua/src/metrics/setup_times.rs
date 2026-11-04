//! How long a new call's turn waits and holds its permits (ADR-0022 X1): the
//! time from the INVITE's receipt to the turn's start, which the setup budget
//! bounds, and the hold that makes a backlog grow past the new-call share.

use std::time::Duration;

use super::catalogue::setup::{BUCKETS, PERMIT_HOLD_SECONDS, TURN_WAIT_SECONDS};
use super::durations::Durations;
use super::B2buaMetrics;

/// The histograms' cells.
#[derive(Debug, Default)]
pub(super) struct SetupTimes {
    turn_wait: Durations<{ BUCKETS.len() }>,
    permit_hold: Durations<{ BUCKETS.len() }>,
}

impl SetupTimes {
    /// Write the turn wait, then the permit hold.
    pub(super) fn render(&self, s: &mut String) {
        self.turn_wait.render(&TURN_WAIT_SECONDS, &BUCKETS, s);
        self.permit_hold.render(&PERMIT_HOLD_SECONDS, &BUCKETS, s);
    }
}

impl B2buaMetrics {
    /// A new call's turn started `waited` after its INVITE's receipt.
    pub fn observe_new_call_turn_wait(&self, waited: Duration) {
        self.inner.setup_times.turn_wait.observe(&BUCKETS, waited);
    }

    /// A new call's turn held its permits for `held`.
    pub fn observe_new_call_permit_hold(&self, held: Duration) {
        self.inner.setup_times.permit_hold.observe(&BUCKETS, held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both histograms read from the first scrape, then count each observation
    /// in every bucket at or above it.
    #[test]
    fn both_histograms_are_readable() {
        let metrics = B2buaMetrics::new();
        let text = metrics.prometheus_text();
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_count 0"), "{text}");
        assert!(text.contains("b2bua_new_call_permit_hold_seconds_count 0"), "{text}");

        metrics.observe_new_call_turn_wait(Duration::from_millis(20));
        metrics.observe_new_call_turn_wait(Duration::from_millis(5_000));
        metrics.observe_new_call_permit_hold(Duration::from_millis(300));
        let text = metrics.prometheus_text();
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_bucket{le=\"0.01\"} 0"), "{text}");
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_bucket{le=\"0.05\"} 1"), "{text}");
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_bucket{le=\"5\"} 2"), "{text}");
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_sum 5.02"), "{text}");
        assert!(text.contains("b2bua_new_call_turn_wait_seconds_count 2"), "{text}");
        assert!(
            text.contains("b2bua_new_call_permit_hold_seconds_bucket{le=\"0.25\"} 0"),
            "{text}"
        );
        assert!(text.contains("b2bua_new_call_permit_hold_seconds_bucket{le=\"0.5\"} 1"), "{text}");
    }
}
