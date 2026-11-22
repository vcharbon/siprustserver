//! [`SlowLedger`]: what the probe remembers of each worker's lateness
//! (ADR-0069): the instant of its last late answer, its run of answers inside
//! their reply windows, and the time it has spent `Slow`.

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
    /// Since when it reads `Slow`, while it does.
    slow_since: Option<Instant>,
    /// Time spent `Slow` in the spells that ended, since the last reset.
    spent: Duration,
    /// Since when it reads `Alive`, while it does.
    alive_since: Option<Instant>,
}

/// Every judged worker's lateness, keyed by ordinal. The time a worker spends
/// `Slow` adds up over its spells, and an `Alive` spell of `slow_max` clears it,
/// as a dead verdict, a restart and a host move do.
#[derive(Debug, Default)]
pub(super) struct SlowLedger {
    workers: HashMap<String, Lateness>,
}

/// What reading a worker's health told the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Spell {
    /// A `Slow` spell began.
    SlowBegan,
    /// The worker is `Slow` and has been for `slow_max` in all.
    SlowTooLong,
    Other,
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

    /// Read `id`'s health at `now`, closing or opening its `Slow` spell, and
    /// say whether a spell began or has lasted `slow_max` in all.
    pub(super) fn read(
        &mut self,
        id: &str,
        health: WorkerHealth,
        now: Instant,
        slow_max: Duration,
    ) -> Spell {
        let w = self.workers.entry(id.to_string()).or_default();
        if health != WorkerHealth::Slow {
            if let Some(since) = w.slow_since.take() {
                w.spent = w.spent.saturating_add(now.saturating_duration_since(since));
            }
        }
        if health != WorkerHealth::Alive {
            w.alive_since = None;
        }
        match health {
            WorkerHealth::Slow => {
                let began = w.slow_since.is_none();
                let since = *w.slow_since.get_or_insert(now);
                let spent = w.spent.saturating_add(now.saturating_duration_since(since));
                if spent >= slow_max {
                    Spell::SlowTooLong
                } else if began {
                    Spell::SlowBegan
                } else {
                    Spell::Other
                }
            }
            WorkerHealth::Alive => {
                let since = *w.alive_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= slow_max {
                    w.spent = Duration::ZERO;
                }
                Spell::Other
            }
            WorkerHealth::Dead => {
                w.spent = Duration::ZERO;
                Spell::Other
            }
            WorkerHealth::Unknown => Spell::Other,
        }
    }

    /// Forget `id`: a new process, or one at a new address.
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

    const MAX: Duration = Duration::from_secs(20);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn slow_time_adds_up_over_spells_and_a_long_alive_spell_clears_it() {
        let t = Instant::now();
        let mut l = SlowLedger::default();
        assert_eq!(l.read("w", WorkerHealth::Slow, t, MAX), Spell::SlowBegan);
        assert_eq!(l.read("w", WorkerHealth::Alive, at(t, 12_000), MAX), Spell::Other);
        assert_eq!(l.read("w", WorkerHealth::Slow, at(t, 15_000), MAX), Spell::SlowBegan);
        assert_eq!(l.read("w", WorkerHealth::Slow, at(t, 22_999), MAX), Spell::Other);
        assert_eq!(l.read("w", WorkerHealth::Slow, at(t, 23_000), MAX), Spell::SlowTooLong);

        let mut l = SlowLedger::default();
        l.read("w", WorkerHealth::Slow, t, MAX);
        l.read("w", WorkerHealth::Alive, at(t, 12_000), MAX);
        l.read("w", WorkerHealth::Alive, at(t, 32_000), MAX);
        l.read("w", WorkerHealth::Slow, at(t, 33_000), MAX);
        assert_eq!(l.read("w", WorkerHealth::Slow, at(t, 52_999), MAX), Spell::Other);
        assert_eq!(l.read("w", WorkerHealth::Slow, at(t, 53_000), MAX), Spell::SlowTooLong);
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
