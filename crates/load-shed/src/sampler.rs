//! The current-load read seam, [`LoadSampler`], and the simulated sampler
//! tests inject readings through.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Clamp a reading to `0..=1`, mapping non-finite to `0`.
pub fn clamp01(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    v.clamp(0.0, 1.0)
}

/// Current-load reader: two snapshot reads, each a `0..=1` ratio over the
/// window since its previous call. Smoothing is the consumer's ([`Ewma`]), not
/// the sampler's, so a test injects a raw value with no convergence wait.
///
/// [`Ewma`]: crate::Ewma
pub trait LoadSampler: Send + Sync {
    /// Event-loop utilization since the previous `elu()` call (`0..=1`): "the
    /// loop is busy", in whatever measure the process takes of it.
    fn elu(&self) -> f64;
    /// Fraction of wall time spent in GC pauses since the previous
    /// `gc_fraction()` call (`0..=1`).
    fn gc_fraction(&self) -> f64;
}

/// A [`LoadSampler`] whose readings a paired [`SimulatedLoadControl`] sets.
/// Build with [`simulated`].
#[derive(Clone)]
pub struct SimulatedLoadSampler {
    inner: Arc<SimulatedInner>,
}

/// The control half of [`SimulatedLoadSampler`]: sets the next reading,
/// clamped to `0..=1`.
#[derive(Clone)]
pub struct SimulatedLoadControl {
    inner: Arc<SimulatedInner>,
}

struct SimulatedInner {
    // Bit patterns of f64s: the read seam is lock-free and a set on another
    // task is seen by the next read.
    elu_bits: AtomicU64,
    gc_bits: AtomicU64,
}

/// A simulated sampler and its control over one shared cell: a value set
/// through the control is read back through the sampler. Both read `0` first.
pub fn simulated() -> (SimulatedLoadSampler, SimulatedLoadControl) {
    let inner = Arc::new(SimulatedInner {
        elu_bits: AtomicU64::new(0.0f64.to_bits()),
        gc_bits: AtomicU64::new(0.0f64.to_bits()),
    });
    (SimulatedLoadSampler { inner: inner.clone() }, SimulatedLoadControl { inner })
}

impl LoadSampler for SimulatedLoadSampler {
    fn elu(&self) -> f64 {
        f64::from_bits(self.inner.elu_bits.load(Ordering::Relaxed))
    }
    fn gc_fraction(&self) -> f64 {
        f64::from_bits(self.inner.gc_bits.load(Ordering::Relaxed))
    }
}

impl SimulatedLoadControl {
    /// Set the next `elu()` reading (clamped to `0..=1`).
    pub fn set_elu(&self, v: f64) {
        self.inner.elu_bits.store(clamp01(v).to_bits(), Ordering::Relaxed);
    }
    /// Set the next `gc_fraction()` reading (clamped to `0..=1`).
    pub fn set_gc_fraction(&self, v: f64) {
        self.inner.gc_bits.store(clamp01(v).to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp01_bounds_and_maps_non_finite_to_zero() {
        assert_eq!(clamp01(0.5), 0.5);
        assert_eq!(clamp01(5.0), 1.0);
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(f64::NAN), 0.0);
        assert_eq!(clamp01(f64::INFINITY), 0.0);
    }

    #[test]
    fn a_fresh_simulated_sampler_reads_zero() {
        let (sampler, _ctl) = simulated();
        assert_eq!(sampler.elu(), 0.0);
        assert_eq!(sampler.gc_fraction(), 0.0);
    }

    /// The control and the sampler share one cell; readings are clamped.
    #[test]
    fn simulated_control_and_sampler_share_one_cell_and_clamp() {
        let (sampler, ctl) = simulated();
        ctl.set_elu(0.85);
        ctl.set_gc_fraction(0.1);
        assert!((sampler.elu() - 0.85).abs() < 1e-9);
        assert!((sampler.gc_fraction() - 0.1).abs() < 1e-9);
        ctl.set_elu(5.0);
        assert_eq!(sampler.elu(), 1.0);
        ctl.set_elu(-1.0);
        assert_eq!(sampler.elu(), 0.0);
        ctl.set_elu(f64::NAN);
        assert_eq!(sampler.elu(), 0.0);
    }

    /// A clone of the sampler reads the same cell.
    #[test]
    fn a_cloned_sampler_reads_the_same_cell() {
        let (sampler, ctl) = simulated();
        let other = sampler.clone();
        ctl.set_elu(0.4);
        assert_eq!(other.elu(), sampler.elu());
    }
}
