//! Band thresholds + hysteresis + AIMD ladder tunables, their shipped
//! defaults, and the band-config validator the runner's boot preflight calls.
//! The classifier these knobs feed lives in [`super::band`].

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
    /// Cap a worker's bucket is created at, on its first OPTIONS reply; the
    /// AIMD step of a reply carrying a payload follows at once.
    pub cap_initial_cps: f64,
    /// Cap never decreases below this.
    pub cap_floor_cps: f64,
    /// Cap never increases above this.
    pub cap_ceiling_cps: f64,
    /// Payload older than this (ms) is stale, and the sweep decreases the cap.
    /// It is at least twice the health probe cycle (the runner preflight refuses
    /// otherwise), or the sweep halves the cap of a worker that answers every
    /// cycle down to the floor.
    pub payload_stale_ms: i64,
    /// Nominal OPTIONS interval (ms) — used for the cooldown clock.
    pub options_interval_ms: i64,
}

impl LoadObserverConfig {
    /// Band-classifier ordering + hysteresis-bounds guard — the invariants
    /// `compute_band` relies on:
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

        if !(self.elu_soft < self.elu_hard && self.elu_hard < self.elu_critical) {
            violations.push(format!(
                "ELU band thresholds must satisfy elu_soft < elu_hard < elu_critical \
                 (got elu_soft={}, elu_hard={}, elu_critical={}).",
                self.elu_soft, self.elu_hard, self.elu_critical,
            ));
        }

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
    /// Aggressive bands: a loaded worker stops taking new non-emergency calls
    /// early, keeping CPU for emergency and in-dialog traffic, and each decrease
    /// tick halves its cap. The band, hysteresis, AIMD and cap fields are set at
    /// boot (`LB_ELU_*`, `LB_BAND_HYSTERESIS`, `LB_AIMD_*`, `LB_CAP_*`) and pass
    /// `validate_bands`. See ADR-0009.
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

    #[test]
    fn default_bands_are_the_aggressive_calibration_starting_points() {
        let c = LoadObserverConfig::default();
        assert_eq!(c.elu_soft, 0.4);
        assert_eq!(c.elu_hard, 0.5, "multiplicative-decrease band opens early");
        assert_eq!(c.elu_critical, 0.65, "worker leaves new-dialog candidacy early");
        assert_eq!(c.aimd_decrease_factor, 0.5, "decrease stays aggressive (×0.5)");
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
        cfg.elu_hard = 0.3;
        cfg.elu_critical = 0.5;
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
