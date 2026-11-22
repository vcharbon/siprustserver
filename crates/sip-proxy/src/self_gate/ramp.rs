//! [`IntakeRamp`]: the share of its rate the self-gate admits at while the
//! POP returns to rotation (ADR-0069): `start_pct` until the first external
//! INVITE after its start, then rising linearly to the whole rate over its
//! span from that INVITE, so a ramp nobody offered traffic to is not spent.

use std::time::Duration;

/// One ramp, on the gate's own timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct IntakeRamp {
    /// The first external INVITE it met; `None` while it waits for one.
    began: Option<Duration>,
    start: f64,
    span: Duration,
}

impl IntakeRamp {
    /// A ramp from `start_pct` percent to the whole rate over `span`; `None`
    /// for a span of zero or a start of the whole rate, which ramp nothing.
    pub(super) fn starting(start_pct: f64, span: Duration) -> Option<Self> {
        let start = (start_pct / 100.0).clamp(0.0, 1.0);
        (!span.is_zero() && start < 1.0).then_some(Self { began: None, start, span })
    }

    /// An external INVITE met the gate at `now`: the span runs from the first.
    pub(super) fn offered(&mut self, now: Duration) {
        self.began.get_or_insert(now);
    }

    /// The share of the rate admitted at `now`, in `[start, 1)`; `None` once
    /// the span has run out.
    pub(super) fn share(&self, now: Duration) -> Option<f64> {
        let Some(began) = self.began else { return Some(self.start) };
        let elapsed = now.saturating_sub(began);
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

    fn near(share: Option<f64>, want: f64) -> bool {
        share.is_some_and(|s| (s - want).abs() < 1e-9)
    }

    #[test]
    fn the_share_holds_its_start_until_offered_then_rises_linearly_to_the_whole() {
        let at = |ms| Duration::from_millis(ms);
        let mut ramp = IntakeRamp::starting(20.0, SPAN).unwrap();
        assert!(near(ramp.share(at(60_000)), 0.2), "nothing offered: the start holds");
        ramp.offered(at(61_000));
        ramp.offered(at(62_000));
        assert!(near(ramp.share(at(61_000)), 0.2));
        assert!(near(ramp.share(at(76_000)), 0.6));
        assert!(ramp.share(at(90_999)).unwrap() < 1.0);
        assert_eq!(ramp.share(at(91_000)), None, "the span ran out from the first INVITE");
    }

    #[test]
    fn a_span_of_zero_or_a_start_of_the_whole_rate_ramps_nothing() {
        assert_eq!(IntakeRamp::starting(20.0, Duration::ZERO), None);
        assert_eq!(IntakeRamp::starting(100.0, SPAN), None);
    }
}
