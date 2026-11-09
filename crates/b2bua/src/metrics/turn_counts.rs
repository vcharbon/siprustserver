//! The counts a turn's notes owe the metrics (ADR-0052 §4), the model
//! oracle's violations (ADR-0052 §8) and the turns held to a per-call cap
//! (ADR-0057 §7).

use b2bua_sdk::config::Cap;

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

    /// Count one turn that crossed `cap`, for `b2bua_cap_trips_total{cap,kind}`.
    pub fn record_cap_trip(&self, cap: Cap) {
        self.inner.rows.cap_trips.add(&[cap.label(), cap.kind().label()], 1);
    }

    /// The turns that crossed `cap`.
    pub fn cap_trips_of(&self, cap: Cap) -> u64 {
        self.inner.rows.cap_trips.get(&[cap.label(), cap.kind().label()])
    }

    /// Count one turn that grew the collection `cap` bounds across its half,
    /// for `b2bua_cap_near_total{cap}`.
    pub fn record_cap_near(&self, cap: Cap) {
        self.inner.rows.cap_near.add(&[cap.label()], 1);
    }

    /// The turns that grew the collection `cap` bounds across its half.
    pub fn cap_near_of(&self, cap: Cap) -> u64 {
        self.inner.rows.cap_near.get(&[cap.label()])
    }

    /// Write the turn counts' families, in exposition order.
    pub(super) fn render_turn_counts(&self, s: &mut String) {
        let rows = &self.inner.rows;
        rows.repeat_give_ups.render(s);
        rows.model_defects.render(s);
        rows.cap_trips.render(s);
        rows.cap_near.render(s);
    }
}
