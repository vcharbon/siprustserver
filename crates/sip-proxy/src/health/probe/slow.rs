//! [`SlowLedger`]: what the probe remembers of each worker's lateness
//! (ADR-0069): the instant of its last late answer, its run of answers inside
//! their reply windows, and whether it reads `Slow`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::registry::WorkerHealth;

/// One worker's lateness.
#[derive(Debug, Default)]
struct Lateness {
    /// The last answer that arrived past its reply window.
    late_at: Option<Instant>,
    /// Consecutive answers inside their reply windows since the last miss.
    timely: u32,
    /// It reads `Slow`.
    slow: bool,
}

/// Every judged worker's lateness, keyed by ordinal.
#[derive(Debug, Default)]
pub(super) struct SlowLedger {
    workers: HashMap<String, Lateness>,
}

impl SlowLedger {
    /// An answer of `id` arrived past its reply window at `now`.
    pub(super) fn answered_late(&mut self, id: &str, now: Instant) {
        self.workers.entry(id.to_string()).or_default().late_at = Some(now);
    }

    /// An answer of `id` arrived inside its reply window: the run it extends.
    pub(super) fn answered_in_time(&mut self, id: &str) -> u32 {
        let w = self.workers.entry(id.to_string()).or_default();
        w.timely = w.timely.saturating_add(1);
        w.timely
    }

    /// A probe of `id` was a miss: its run of answers in time ends.
    pub(super) fn missed(&mut self, id: &str) {
        if let Some(w) = self.workers.get_mut(id) {
            w.timely = 0;
        }
    }

    /// Whether `id` answered late within the `window` before `now`.
    pub(super) fn late_within(&self, id: &str, now: Instant, window: Duration) -> bool {
        self.workers
            .get(id)
            .and_then(|w| w.late_at)
            .is_some_and(|at| now.saturating_duration_since(at) <= window)
    }

    /// Read `id`'s health: whether a `Slow` spell began with it.
    pub(super) fn read(&mut self, id: &str, health: WorkerHealth) -> bool {
        let w = self.workers.entry(id.to_string()).or_default();
        let slow = health == WorkerHealth::Slow;
        let began = slow && !w.slow;
        w.slow = slow;
        began
    }

    /// Forget `id`: a worker at a new address.
    pub(super) fn forget(&mut self, id: &str) {
        self.workers.remove(id);
    }

    pub(super) fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.workers.retain(|id, _| keep(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_spell_begins_once_per_turn_to_slow() {
        let mut l = SlowLedger::default();
        assert!(l.read("w", WorkerHealth::Slow));
        assert!(!l.read("w", WorkerHealth::Slow));
        assert!(!l.read("w", WorkerHealth::Alive));
        assert!(l.read("w", WorkerHealth::Slow));
    }

    #[test]
    fn a_miss_ends_the_run_of_answers_in_time() {
        let mut l = SlowLedger::default();
        assert_eq!(l.answered_in_time("w"), 1);
        assert_eq!(l.answered_in_time("w"), 2);
        l.missed("w");
        assert_eq!(l.answered_in_time("w"), 1);
    }

    #[test]
    fn a_late_answer_counts_within_its_window_its_last_instant_included() {
        let t = Instant::now();
        let mut l = SlowLedger::default();
        let window = Duration::from_millis(2_000);
        assert!(!l.late_within("w", t, window));
        l.answered_late("w", t);
        assert!(l.late_within("w", at(t, 2_000), window));
        assert!(!l.late_within("w", at(t, 2_001), window));
    }
}
