//! [`IntakeRamp`]: the share of its rate the self-gate admits at while the
//! POP returns to rotation (ADR-0069): `start_pct` at its start, rising
//! linearly to the whole rate over its span.

use std::time::Duration;

/// One ramp, on the gate's own timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct IntakeRamp {
    began: Duration,
    start: f64,
    span: Duration,
}

impl IntakeRamp {
    /// A ramp from `start_pct` percent at `began` to the whole rate `span`
    /// later; `None` for a span of zero, which ramps nothing.
    pub(super) fn starting(began: Duration, start_pct: f64, span: Duration) -> Option<Self> {
        (!span.is_zero()).then_some(Self {
            began,
            start: (start_pct / 100.0).clamp(0.0, 1.0),
            span,
        })
    }

    /// The share of the rate admitted at `now`, in `[start, 1)`; `None` once
    /// the span has run out.
    pub(super) fn share(&self, now: Duration) -> Option<f64> {
        let elapsed = now.saturating_sub(self.began);
        if elapsed >= self.span {
            return None;
        }
        let done = elapsed.as_secs_f64() / self.span.as_secs_f64();
        Some(self.start + (1.0 - self.start) * done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPAN: Duration = Duration::from_secs(30);

    #[test]
    fn the_share_rises_linearly_from_its_start_to_the_whole_rate() {
        let at = |ms| Duration::from_millis(ms);
        let ramp = IntakeRamp::starting(at(1_000), 20.0, SPAN).unwrap();
        let near = |share: Option<f64>, want: f64| share.is_some_and(|s| (s - want).abs() < 1e-9);
        assert!(near(ramp.share(at(1_000)), 0.2));
        assert!(near(ramp.share(at(16_000)), 0.6));
        assert!(ramp.share(at(30_999)).unwrap() < 1.0);
        assert_eq!(ramp.share(at(31_000)), None, "the span ran out");
    }

    #[test]
    fn a_span_of_zero_ramps_nothing() {
        assert_eq!(IntakeRamp::starting(Duration::ZERO, 20.0, Duration::ZERO), None);
    }
}
