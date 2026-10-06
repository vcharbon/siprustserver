//! Exponential moving average, the smoothing applied to a load reading.

/// Each observation blends in as `alpha * sample + (1 - alpha) * value`;
/// `alpha = 0.2` is a ~5-sample window.
///
/// Built with [`new`](Ewma::new) it reads exactly `0` until the first
/// observation, which seats it at the sample, so a reader that has never
/// sampled sees no load. Built with [`starting_at`](Ewma::starting_at), the
/// first observation blends into the given value instead.
#[derive(Debug, Clone, Copy)]
pub struct Ewma {
    value: f64,
    alpha: f64,
    seated: bool,
}

impl Ewma {
    /// An average that reads `0` until its first observation seats it.
    pub fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha, seated: false }
    }

    /// An average already at `value`: every observation, the first included,
    /// blends into it.
    pub fn starting_at(alpha: f64, value: f64) -> Self {
        Self { value, alpha, seated: true }
    }

    /// Fold one sample in.
    pub fn observe(&mut self, sample: f64) {
        if self.seated {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        } else {
            self.value = sample;
            self.seated = true;
        }
    }

    /// The current average.
    pub fn get(&self) -> f64 {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_zero_until_the_first_observation_seats_it() {
        let mut e = Ewma::new(0.2);
        assert_eq!(e.get(), 0.0);
        e.observe(0.9);
        assert_eq!(e.get(), 0.9, "the first sample seats the average, no blend from 0");
    }

    #[test]
    fn later_observations_blend_toward_the_sample_never_past_it() {
        let mut e = Ewma::new(0.2);
        e.observe(1.0);
        e.observe(0.0);
        assert!((e.get() - 0.8).abs() < 1e-12);
        e.observe(0.0);
        assert!((e.get() - 0.64).abs() < 1e-12);
    }

    #[test]
    fn an_average_starting_at_a_value_blends_its_first_observation() {
        let mut e = Ewma::starting_at(0.3, 0.0);
        e.observe(30.0);
        assert!((e.get() - 9.0).abs() < 1e-12, "0.7 × 0 + 0.3 × 30");
    }
}
