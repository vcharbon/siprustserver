//! The stall detector that turns a loop making no progress into a diagnosis.

use std::time::Duration;

use pivot_schema::bundle::Failure;
use tokio::time::Instant;

/// A stall detector: how many consecutive turns a loop has taken without the
/// clock moving, and how much wall time it has burned.
///
/// A turn that neither advances virtual time nor changes the run's position is
/// a turn that will repeat forever. The bounds are generous — a busy settle
/// window handles many datagrams inside one virtual instant, and a real clock
/// spends its document's whole declared timeline — and their only job is to turn
/// a hang into a diagnosis.
pub(super) struct Stall {
    last: Instant,
    started: std::time::Instant,
    /// Wall time this run may burn, derived from its clock and the timeline its
    /// document declares.
    wall: Duration,
    turns: u32,
    total: u64,
}

impl Stall {
    /// Turns at ONE instant before a loop is declared stuck: neither the clock
    /// nor the run is moving.
    const LIMIT: u32 = 5_000;
    /// Turns in TOTAL before a loop is declared stuck: the clock is moving, in
    /// steps too small to ever reach a deadline. Generous — a long call polls
    /// its settle window thousands of times.
    const TOTAL: u64 = 2_000_000;

    pub(super) fn new(wall: Duration) -> Self {
        Stall { last: Instant::now(), started: std::time::Instant::now(), wall, turns: 0, total: 0 }
    }

    /// `Some` once the loop cannot be making progress, by any of the three
    /// measures. The failure names the phase and what the run was waiting for.
    pub(super) fn check(
        &mut self,
        phase: &str,
        waiting_on: impl FnOnce() -> Vec<String>,
    ) -> Option<Failure> {
        let now = Instant::now();
        self.total += 1;
        if now != self.last {
            self.last = now;
            self.turns = 0;
        } else {
            self.turns += 1;
        }
        let stuck = self.turns > Self::LIMIT
            || self.total > Self::TOTAL
            || self.started.elapsed() > self.wall;
        if !stuck {
            return None;
        }
        let mut detail = waiting_on();
        detail.push(format!(
            "after {} turn(s), {} at one instant, {:?} of wall time",
            self.total,
            self.turns,
            self.started.elapsed()
        ));
        Some(Failure::RunStalled { phase: phase.to_string(), waiting_on: detail })
    }
}
