//! Band thresholds + hysteresis + AIMD ladder tunables, their shipped
//! defaults, and the band-config validator the runner's boot preflight calls.
//! The classifier these knobs feed lives in [`super::band`].

/// Band thresholds + hysteresis + AIMD ladder tunables.
#[derive(Debug, Clone, Copy)]
pub struct LoadObserverConfig {
    /// AIMD increase enabled while `elu <= elu_soft`.
    pub elu_soft: f64,
    /// Multiplicative decrease while `elu > elu_hard`.
    pub elu_hard: f64,
    /// Worker filtered out of new-dialog candidates while `elu > elu_critical`.
    pub elu_critical: f64,
    /// Hysteresis applied at every band boundary (exit threshold = enter − h).
    pub band_hysteresis: f64,
    /// Additive increase step per OPTIONS tick when below soft.
    pub aimd_increase_step_cps: f64,
    /// Multiplicative decrease factor when above hard (e.g. `0.75` = ×0.75).
    pub aimd_decrease_factor: f64,
    /// No increases for this many OPTIONS ticks after any decrease.
    pub aimd_cooldown_ticks: f64,
    /// Cap each worker starts at before any payload has arrived.
    pub cap_initial_cps: f64,
    /// Cap never decreases below this.
    pub cap_floor_cps: f64,
    /// Cap never increases above this.
    pub cap_ceiling_cps: f64,
    /// Payload older than this (ms) is stale; sweep conservatively decreases.
    pub payload_stale_ms: i64,
    /// Nominal OPTIONS interval (ms) — used for the cooldown clock.
    pub options_interval_ms: i64,
}

impl LoadObserverConfig {
    /// Band-classifier ordering + hysteresis-bounds guard — the invariants
    /// [`compute_band`](super::band::compute_band) relies on:
    ///
    ///   - `elu_soft < elu_hard < elu_critical` — a reversed band threshold turns
    ///     the controller inside-out (a low ELU lands in `AboveCritical` and the
    ///     worker is filtered out of new-dialog selection at idle).
    ///   - `band_hysteresis ∈ [0, min band gap)` — a hysteresis wider than the
    ///     narrower of the two band gaps means `compute_band` can never exit the
    ///     higher band (the hold-on-decrease arm always re-enters it), so the
    ///     worker is trapped one band high even at zero ELU.
    ///
    /// Pure: appends a human-readable line per violation to `violations` (so a
    /// caller can collect every config problem in one boot attempt) and returns
    /// whether the band config is well-formed. The runner's boot preflight
    /// (`sip-proxy-runner`) calls this as part of its full cross-component check.
    pub fn validate_bands(&self, violations: &mut Vec<String>) -> bool {
        let start = violations.len();

        // ELU band ordering. A reversed band threshold turns the controller
        // inside-out (low ELU triggers above_critical).
        if !(self.elu_soft < self.elu_hard && self.elu_hard < self.elu_critical) {
            violations.push(format!(
                "ELU band thresholds must satisfy elu_soft < elu_hard < elu_critical \
                 (got elu_soft={}, elu_hard={}, elu_critical={}).",
                self.elu_soft, self.elu_hard, self.elu_critical,
            ));
        }

        // Hysteresis wider than a band gap means `compute_band` cannot exit the
        // higher band even at zero ELU.
        let min_band_gap = (self.elu_hard - self.elu_soft).min(self.elu_critical - self.elu_hard);
        if self.band_hysteresis < 0.0 || self.band_hysteresis >= min_band_gap {
            violations.push(format!(
                "band_hysteresis ({}) must be in [0, min band gap) — min gap is {}. \
                 A hysteresis wider than a band traps the controller in the higher \
                 band even at zero ELU.",
                self.band_hysteresis, min_band_gap,
            ));
        }

        violations.len() == start
    }
}

impl Default for LoadObserverConfig {
    /// Deliberately aggressive bands: a loaded worker stops taking new
    /// non-emergency traffic early so it reserves CPU for emergency +
    /// already-established (in-dialog) calls, and each decrease tick halves the
    /// cap (`aimd_decrease_factor` 0.5).
    ///
    /// **These are CALIBRATION STARTING POINTS, not final values.** They are to
    /// be tuned empirically against the cluster overload sweep (the all-worker
    /// 0.30-core `overloadall` case); the runner exposes every band/AIMD field as
    /// an env var (`LB_ELU_*`, `LB_AIMD_*`, `LB_CAP_*`) so the sweep can retune
    /// without a rebuild. Any change must keep `elu_soft < elu_hard < elu_critical`
    /// and `band_hysteresis < min band gap` (here min gap = 0.5−0.4 = 0.1 >
    /// 0.05) so `validate_bands` / the runner preflight accept the config.
    ///
    /// At 2 workers × ~50 CAPS sustained, `cap_initial_cps = 30` admits 30 cps per
    /// worker initially and AIMDs up while ELU stays low; a burst drains the
    /// bucket and the per-tick decreases collapse it further. `payload_stale_ms`
    /// (8000) MUST exceed one full HealthProbe cycle (`interval_ms + timeout_ms`)
    /// or `sweep_stale` halves the cap every cycle down to the floor, silently —
    /// the runner preflight enforces a 2× margin.
    fn default() -> Self {
        Self {
            elu_soft: 0.4,
            elu_hard: 0.5,
            elu_critical: 0.65,
            band_hysteresis: 0.05,
            aimd_increase_step_cps: 2.0,
            aimd_decrease_factor: 0.5,
            aimd_cooldown_ticks: 5.0,
            cap_initial_cps: 30.0,
            cap_floor_cps: 1.0,
            cap_ceiling_cps: 200.0,
            payload_stale_ms: 8000,
            options_interval_ms: 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_bands_accepts_shipped_defaults() {
        let mut v = Vec::new();
        assert!(LoadObserverConfig::default().validate_bands(&mut v));
        assert!(v.is_empty());
    }

    /// Pin the aggressive shipped band thresholds (a loaded worker sheds
    /// non-emergency sooner). These are calibration starting points; the runner
    /// exposes them as env vars (`LB_ELU_*`) so the cluster sweep can retune
    /// without a rebuild. The test guards an accidental revert and a retuning
    /// that would break `validate_bands` (it asserts the ordering +
    /// hysteresis-bound invariants hold for the shipped values).
    #[test]
    fn default_bands_are_the_aggressive_calibration_starting_points() {
        let c = LoadObserverConfig::default();
        assert_eq!(c.elu_soft, 0.4);
        assert_eq!(c.elu_hard, 0.5, "multiplicative-decrease band opens early");
        assert_eq!(c.elu_critical, 0.65, "worker leaves new-dialog candidacy early");
        assert_eq!(c.aimd_decrease_factor, 0.5, "decrease stays aggressive (×0.5)");
        // Invariants that keep validate_bands / the runner preflight happy.
        assert!(c.elu_soft < c.elu_hard && c.elu_hard < c.elu_critical);
        let min_gap = (c.elu_hard - c.elu_soft).min(c.elu_critical - c.elu_hard);
        assert!(
            c.band_hysteresis < min_gap,
            "hysteresis {} must be < min gap {}",
            c.band_hysteresis,
            min_gap
        );
    }

    #[test]
    fn validate_bands_rejects_elu_soft_ge_elu_hard() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.6;
        cfg.elu_hard = 0.6;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("elu_soft < elu_hard")));
    }

    #[test]
    fn validate_bands_rejects_elu_hard_ge_elu_critical() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_hard = 0.75;
        cfg.elu_critical = 0.75;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    /// Default gaps are hard−soft = 0.1 and critical−hard = 0.15 → min gap 0.1;
    /// hysteresis 0.25 traps the controller in the higher band.
    #[test]
    fn validate_bands_rejects_hysteresis_wider_than_a_band_gap() {
        let mut cfg = LoadObserverConfig::default();
        cfg.band_hysteresis = 0.25;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("band_hysteresis")));
    }

    /// Hysteresis exactly equal to the min band gap is rejected (half-open
    /// upper bound `[0, min gap)`): the exit threshold then coincides with the
    /// lower band's enter threshold, so `compute_band` never leaves the band.
    /// Uses thresholds whose gaps are exactly f64-representable so the boundary
    /// is tested cleanly (0.75−0.6 is 0.15000000000000002 in f64, which would
    /// make `band_hysteresis = 0.15` strictly *less* than the min gap and admit).
    #[test]
    fn validate_bands_rejects_hysteresis_equal_to_min_gap() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.1;
        cfg.elu_hard = 0.3; // hard−soft = 0.2
        cfg.elu_critical = 0.5; // critical−hard = 0.2 → min gap = 0.2
        cfg.band_hysteresis = 0.2;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    #[test]
    fn validate_bands_rejects_negative_hysteresis() {
        let mut cfg = LoadObserverConfig::default();
        cfg.band_hysteresis = -0.01;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
    }

    /// A fully reversed ordering trips the ordering rule (it does not silently
    /// pass). Both elu ordering and hysteresis errors can surface together.
    #[test]
    fn validate_bands_rejects_fully_reversed_ordering() {
        let mut cfg = LoadObserverConfig::default();
        cfg.elu_soft = 0.9;
        cfg.elu_hard = 0.6;
        cfg.elu_critical = 0.3;
        let mut v = Vec::new();
        assert!(!cfg.validate_bands(&mut v));
        assert!(v.iter().any(|m| m.contains("elu_soft < elu_hard < elu_critical")));
    }
}
