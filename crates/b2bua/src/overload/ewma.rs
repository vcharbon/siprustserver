//! Exponential-moving-average smoothing for the published load readings.

/// Simple EWMA. `alpha = 0.2` gives a ~5-sample smoothing window. Stays exactly
/// `0` until the first `observe`, so the published header reads `elu=0.000`
/// before the sampler has fired (pinned by the schema test).
#[derive(Debug, Clone, Copy)]
pub(super) struct Ewma {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    pub(super) fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha, initialized: false }
    }
    /// First observation seats the EWMA exactly at the sample; later ones blend
    /// `alpha * sample + (1 - alpha) * value`.
    pub(super) fn observe(&mut self, sample: f64) {
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        }
    }
    pub(super) fn get(&self) -> f64 {
        self.value
    }
}
