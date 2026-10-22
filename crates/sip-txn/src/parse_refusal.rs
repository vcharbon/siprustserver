//! The cap on the stateless 400s that answer requests the parser refused: one
//! token bucket per ingress, so a flood of malformed requests (whose source
//! address may be forged) draws a bounded rate of answers.

use std::time::Duration;

use load_shed::TokenBucket;

/// The 400s an ingress sends back to back before the rate binds.
pub const PARSE_REFUSAL_BURST: u32 = 100;

/// The 400s per second an ingress sends once its burst is spent.
pub const PARSE_REFUSAL_PER_SEC: u32 = 100;

/// A full bucket of [`PARSE_REFUSAL_BURST`] tokens refilling at
/// [`PARSE_REFUSAL_PER_SEC`], its time credited up to `now`.
pub fn parse_refusal_bucket(now: Duration) -> TokenBucket {
    TokenBucket::full(f64::from(PARSE_REFUSAL_BURST), f64::from(PARSE_REFUSAL_PER_SEC), now)
}
