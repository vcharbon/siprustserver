//! Call limiter seam — port target `CallLimiter` (row 20, a separate future
//! migration layer). This slice ships only the trait + a no-op implementation
//! that always admits; the soft `decrement-limiter` effect is a no-op. The real
//! limiter (sliding windows over a shared store) lands with its own slice.

use async_trait::async_trait;

/// Admission/decrement seam for per-call rate limits.
#[async_trait]
pub trait CallLimiter: Send + Sync {
    /// Try to admit one call under `limiter_id` with `limit`. `true` = admitted.
    async fn check_and_increment(&self, limiter_id: &str, limit: i64) -> bool;
    /// Release one admission from the given window.
    async fn decrement(&self, limiter_id: &str, window: i64);
}

/// Always admits; decrements are no-ops.
#[derive(Clone, Default)]
pub struct NoopLimiter;

#[async_trait]
impl CallLimiter for NoopLimiter {
    async fn check_and_increment(&self, _limiter_id: &str, _limit: i64) -> bool {
        true
    }
    async fn decrement(&self, _limiter_id: &str, _window: i64) {}
}
