//! ELU band classification with hysteresis. The thresholds live on
//! [`LoadObserverConfig`](super::config::LoadObserverConfig); its
//! `validate_bands` guards exactly the invariants this walk relies on
//! (ordered thresholds, hysteresis narrower than every band gap).

use super::config::LoadObserverConfig;

/// AIMD band derived from `elu` with hysteresis on transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EluBand {
    BelowSoft,
    SoftToHard,
    HardToCritical,
    /// Filtered out of non-emergency new-dialog candidates.
    AboveCritical,
}

/// Walk down the bands with hysteresis: once in a higher band, `elu` must drop
/// below `enter − h` before transitioning out.
pub(crate) fn compute_band(c: &LoadObserverConfig, elu: f64, prev: EluBand) -> EluBand {
    let h = c.band_hysteresis;
    if elu > c.elu_critical || (prev == EluBand::AboveCritical && elu > c.elu_critical - h) {
        return EluBand::AboveCritical;
    }
    if elu > c.elu_hard || (prev == EluBand::HardToCritical && elu > c.elu_hard - h) {
        return EluBand::HardToCritical;
    }
    if elu > c.elu_soft || (prev == EluBand::SoftToHard && elu > c.elu_soft - h) {
        return EluBand::SoftToHard;
    }
    EluBand::BelowSoft
}
