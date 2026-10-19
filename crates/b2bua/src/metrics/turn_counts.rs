//! The counts a turn's notes owe the metrics (ADR-0052 §4) and the model
//! oracle's violations (ADR-0052 §8).

use super::B2buaMetrics;

impl B2buaMetrics {
    /// Count one dialog-level ladder that ran to its give-up, for
    /// `b2bua_repeat_give_ups_total{obligation}`.
    pub fn record_repeat_give_up(&self, obligation: &str) {
        self.inner.rows.repeat_give_ups.add(&[obligation], 1);
    }

    /// The give-ups of one obligation kind.
    pub fn repeat_give_ups_total(&self, obligation: &str) -> u64 {
        self.inner.rows.repeat_give_ups.get(&[obligation])
    }

    /// Count one model invariant broken or defect met, for
    /// `b2bua_model_defects_total{invariant}`.
    pub fn record_model_defect(&self, invariant: &str) {
        self.inner.rows.model_defects.add(&[invariant], 1);
    }

    /// The violations of one model invariant or defect.
    pub fn model_defects_total(&self, invariant: &str) -> u64 {
        self.inner.rows.model_defects.get(&[invariant])
    }
}
