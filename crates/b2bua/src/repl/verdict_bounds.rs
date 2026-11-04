//! [`VerdictBounds`] — how a worker reads a front proxy's dead verdicts
//! (ADR-0050 §3): how far a verdict's instant may stand from the worker's
//! clock, and how long a taken verdict is held so a repeat of its notice is
//! read as the same verdict.

use sip_txn::timers::TIMER_F;

/// The default freshness window, ms: a notice whose verdict instant stands
/// further from this node's clock, either way, is refused as stale.
pub const DEFAULT_FRESH_MS: i64 = 10_000;

/// The default hold of a taken verdict, ms: the freshness window plus Timer F
/// (64·T1), the last a repeat of its notice can arrive.
pub const DEFAULT_HOLD_MS: i64 = DEFAULT_FRESH_MS + TIMER_F.cast_signed();

/// The dead-verdict bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerdictBounds {
    /// How far a verdict's instant may stand from this node's clock, either
    /// way, ms: a notice past it is refused and counted
    /// (`b2bua_dead_notices_refused_total`), so a replayed notice is refused
    /// once the window passes.
    pub fresh_ms: i64,
    /// How long a taken verdict is held, ms: a repeat of its notice inside it
    /// (the proxy's OPTIONS repeats on Timer E up to Timer F) is a duplicate,
    /// neither checked again nor refused; a verdict on the same worker at
    /// another instant sweeps again. At least the freshness window plus
    /// Timer F, so no repeat a fresh notice draws is read as a new verdict.
    pub hold_ms: i64,
}

impl Default for VerdictBounds {
    fn default() -> Self {
        Self { fresh_ms: DEFAULT_FRESH_MS, hold_ms: DEFAULT_HOLD_MS }
    }
}

impl VerdictBounds {
    /// `Err` naming the bound out of range: a freshness window not above
    /// zero, or a hold under the freshness window plus Timer F.
    pub fn validate(&self) -> Result<(), String> {
        if self.fresh_ms <= 0 {
            return Err(format!("the freshness window {} ms is not above zero", self.fresh_ms));
        }
        let floor = self.fresh_ms.saturating_add(TIMER_F.cast_signed());
        if self.hold_ms < floor {
            return Err(format!(
                "the verdict hold {} ms is under the freshness window {} ms plus Timer F \
                 ({} ms)",
                self.hold_ms, self.fresh_ms, TIMER_F
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_holds_a_verdict_through_its_last_repeat() {
        let d = VerdictBounds::default();
        assert_eq!((d.fresh_ms, d.hold_ms), (10_000, 42_000));
        assert_eq!(d.validate(), Ok(()));
    }

    #[test]
    fn a_hold_under_the_window_plus_timer_f_is_refused() {
        let short = VerdictBounds { fresh_ms: 10_000, hold_ms: 41_999 };
        assert!(short.validate().unwrap_err().contains("freshness window"));
        assert!(VerdictBounds { fresh_ms: 0, hold_ms: 42_000 }.validate().is_err());
    }
}
