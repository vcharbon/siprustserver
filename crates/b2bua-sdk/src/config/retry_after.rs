//! [`RetryAfterRange`]: the `Retry-After` a worker states, drawn from a range.

/// An inclusive range of `Retry-After` seconds (RFC 3261 §20.33), one value
/// drawn uniformly per response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryAfterRange {
    pub min_sec: u32,
    pub max_sec: u32,
}

impl RetryAfterRange {
    /// The value `roll` (any `u64`) picks, uniform over `[min_sec, max_sec]`.
    #[expect(clippy::cast_possible_truncation, reason = "the remainder is below span ≤ 2^32")]
    pub fn pick(&self, roll: u64) -> u32 {
        let span = u64::from(self.max_sec.saturating_sub(self.min_sec)) + 1;
        self.min_sec.saturating_add((roll % span) as u32)
    }
}
