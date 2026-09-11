//! The per-call trace admission chain (ADR-0026).
//!
//! Every entry path — the default draw, an `X-Trace-Sample` header override,
//! a decision-engine `trace: true` force-enable — passes the SAME chain:
//!
//! 1. an exporter must be configured, else the machinery is inert;
//! 2. the Bernoulli draw at the effective rate (a force-enable draws at 1.0);
//! 3. the activation token bucket (burst 10, refill 1/s);
//! 4. the concurrent active-trace cap.
//!
//! A refusal bumps a counter and returns [`Denied`] — it never logs, and it
//! never allocates. Admission yields a [`TraceLease`] whose drop releases the
//! active slot, so the cap tracks live root spans exactly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::counters;
use crate::rate_draw::RateDraw;
use crate::token_bucket::TokenBucket;

/// Concurrent traced calls a process holds open by default.
pub const DEFAULT_MAX_ACTIVE: usize = 200;

/// Why an activation attempt was refused. Each variant maps to one counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// No `OTEL_EXPORTER_OTLP_ENDPOINT`: the sampling machinery is inert.
    NoExporter,
    /// The Bernoulli draw at the effective rate came up negative.
    Draw,
    /// The activation token bucket is empty.
    Rate,
    /// The concurrent active-trace cap is reached.
    ActiveCap,
}

/// The process-wide activation gate. Held in an `Arc` because a granted
/// [`TraceLease`] outlives the call site that took it.
pub struct SampleAdmission {
    exporter_configured: bool,
    default_rate: f64,
    max_active: usize,
    draw: RateDraw,
    bucket: TokenBucket,
    active: AtomicUsize,
}

impl SampleAdmission {
    /// Production shape: entropy-seeded draw, the ADR-0026 default bucket and
    /// cap, `exporter_configured` read once from the environment.
    pub fn from_env(now_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            exporter_configured: crate::exporter_configured(),
            default_rate: crate::DEFAULT_SAMPLE_RATE,
            max_active: DEFAULT_MAX_ACTIVE,
            draw: RateDraw::from_entropy(),
            bucket: TokenBucket::default_at(now_ms),
            active: AtomicUsize::new(0),
        })
    }

    /// A fully-specified gate for tests: deterministic draw, explicit bucket
    /// and cap.
    pub fn new(
        exporter_configured: bool,
        default_rate: f64,
        max_active: usize,
        draw: RateDraw,
        bucket: TokenBucket,
    ) -> Arc<Self> {
        Arc::new(Self {
            exporter_configured,
            default_rate,
            max_active,
            draw,
            bucket,
            active: AtomicUsize::new(0),
        })
    }

    /// The configured default rate, used whenever no override applies.
    pub fn default_rate(&self) -> f64 {
        self.default_rate
    }

    /// Whether this process exports at all.
    pub fn exporter_configured(&self) -> bool {
        self.exporter_configured
    }

    /// Traces currently open.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Run the chain for one call. `rate_override` is the `X-Trace-Sample`
    /// value when the process honors the header and it read as a float in
    /// `0..=1`; `None` uses the configured rate. A decision-engine force-enable
    /// passes `Some(1.0)` — it skips the draw, never the bucket or the cap.
    pub fn admit(
        self: &Arc<Self>,
        rate_override: Option<f64>,
        now_ms: i64,
    ) -> Result<TraceLease, Denied> {
        if !self.exporter_configured {
            counters::bump(&counters::TRACE_DROPPED_NO_EXPORTER);
            return Err(Denied::NoExporter);
        }
        if !self.draw.draw(rate_override.unwrap_or(self.default_rate)) {
            return Err(Denied::Draw);
        }
        if !self.bucket.try_take(now_ms) {
            counters::bump(&counters::TRACE_DENIED_RATE);
            return Err(Denied::Rate);
        }
        if !self.take_slot() {
            counters::bump(&counters::TRACE_DENIED_ACTIVE_CAP);
            return Err(Denied::ActiveCap);
        }
        counters::bump(&counters::TRACE_ADMITTED);
        Ok(TraceLease { gate: self.clone() })
    }

    /// Claim one active-trace slot without exceeding the cap.
    fn take_slot(&self) -> bool {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_active).then_some(n + 1)
            })
            .is_ok()
    }
}

/// One admitted call's active-trace slot. Held for the lifetime of the call's
/// root span; dropping it at span close returns the slot.
pub struct TraceLease {
    gate: Arc<SampleAdmission>,
}

impl Drop for TraceLease {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_on(max_active: usize) -> Arc<SampleAdmission> {
        SampleAdmission::new(true, 1.0, max_active, RateDraw::seeded(1), TokenBucket::default_at(0))
    }

    #[test]
    fn without_an_exporter_nothing_is_admitted() {
        let gate =
            SampleAdmission::new(false, 1.0, 1000, RateDraw::seeded(1), TokenBucket::default_at(0));
        assert_eq!(gate.admit(None, 0).err(), Some(Denied::NoExporter));
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn a_negative_draw_stops_before_the_bucket() {
        let gate =
            SampleAdmission::new(true, 0.0, 1000, RateDraw::seeded(1), TokenBucket::default_at(0));
        for _ in 0..100 {
            assert_eq!(gate.admit(None, 0).err(), Some(Denied::Draw));
        }
        // Nothing consumed the bucket, so a forced call still gets the burst.
        assert!(gate.admit(Some(1.0), 0).is_ok());
    }

    #[test]
    fn the_bucket_paces_activations_even_when_every_draw_wins() {
        let gate = always_on(1000);
        let leases: Vec<TraceLease> = (0..10).map(|_| gate.admit(None, 0).unwrap()).collect();
        assert_eq!(leases.len(), 10);
        assert_eq!(gate.admit(None, 0).err(), Some(Denied::Rate));
        assert!(gate.admit(None, 1000).is_ok(), "a second of refill grants one more");
    }

    #[test]
    fn the_active_cap_holds_and_a_closed_span_returns_its_slot() {
        let gate = always_on(3);
        // Refill keeps the bucket out of the way; the cap is the subject.
        let a = gate.admit(None, 0).unwrap();
        let b = gate.admit(None, 1000).unwrap();
        let c = gate.admit(None, 2000).unwrap();
        assert_eq!(gate.active(), 3);
        assert_eq!(gate.admit(None, 3000).err(), Some(Denied::ActiveCap));
        drop(b);
        assert_eq!(gate.active(), 2);
        let d = gate.admit(None, 4000).unwrap();
        assert_eq!(gate.active(), 3);
        drop((a, c, d));
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn a_force_enable_still_passes_the_bucket_and_the_cap() {
        let gate =
            SampleAdmission::new(true, 0.0, 2, RateDraw::seeded(9), TokenBucket::new(2.0, 1.0, 0));
        let _a = gate.admit(Some(1.0), 0).unwrap();
        let _b = gate.admit(Some(1.0), 0).unwrap();
        assert_eq!(gate.admit(Some(1.0), 0).err(), Some(Denied::Rate));
        assert_eq!(
            gate.admit(Some(1.0), 5000).err(),
            Some(Denied::ActiveCap),
            "with the bucket refilled the cap is what refuses"
        );
    }
}
