//! The slow axis of worker health (ADR-0069): each routed worker's state, and
//! per worker its late probe answers, its `Slow` spells and the dead verdicts
//! a spell too long draws. The worker label is drawn from the pool, which
//! bounds it.

use metric_values::OpenRows;

use super::super::catalogue::{self as c, HEALTH_STATES};
use super::ProxyMetrics;
use crate::registry::WorkerHealth;

pub(super) struct SlowCounts {
    states: OpenRows,
    slow: OpenRows,
    late: OpenRows,
    slow_max_dead: OpenRows,
}

impl Default for SlowCounts {
    fn default() -> Self {
        Self {
            states: OpenRows::new(&c::WORKER_STATE),
            slow: OpenRows::new(&c::WORKER_SLOW),
            late: OpenRows::new(&c::PROBE_LATE_ANSWERS),
            slow_max_dead: OpenRows::new(&c::SLOW_MAX_DEAD),
        }
    }
}

impl SlowCounts {
    pub(super) fn render(&self, s: &mut String) {
        self.states.render(s);
        self.slow.render(s);
        self.late.render(s);
        self.slow_max_dead.render(s);
    }
}

/// The label a health state is exposed under.
pub fn health_label(health: WorkerHealth) -> &'static str {
    let at = match health {
        WorkerHealth::Alive => 0,
        WorkerHealth::Slow => 1,
        WorkerHealth::Unknown => 2,
        WorkerHealth::Dead => 3,
    };
    HEALTH_STATES.get(at).copied().unwrap_or("unknown")
}

impl ProxyMetrics {
    /// Publish every routed worker's health state, a census: a worker absent
    /// from `workers` drops out.
    pub fn set_worker_states<'a>(
        &self,
        workers: impl IntoIterator<Item = (&'a str, WorkerHealth)>,
    ) {
        let rows: Vec<(Vec<&str>, u64)> = workers
            .into_iter()
            .flat_map(|(id, health)| {
                HEALTH_STATES
                    .iter()
                    .map(move |state| (vec![id, *state], u64::from(*state == health_label(health))))
            })
            .collect();
        self.slow.states.replace(rows);
    }

    /// One `Slow` spell of `worker` began.
    pub fn record_worker_slow(&self, worker: &str) {
        self.slow.slow.add_pinned(&[worker], 1);
    }

    /// One probe answer of `worker` arrived late, inside its slow window.
    pub fn record_probe_late_answer(&self, worker: &str) {
        self.slow.late.add_pinned(&[worker], 1);
    }

    /// `worker` read `Dead` for the time it spent `Slow` in all.
    pub fn record_slow_max_dead(&self, worker: &str) {
        self.slow.slow_max_dead.add_pinned(&[worker], 1);
    }

    pub fn worker_slow(&self, worker: &str) -> u64 {
        self.slow.slow.get(&[worker])
    }

    pub fn probe_late_answers(&self, worker: &str) -> u64 {
        self.slow.late.get(&[worker])
    }

    pub fn slow_max_dead(&self, worker: &str) -> u64 {
        self.slow.slow_max_dead.get(&[worker])
    }
}
