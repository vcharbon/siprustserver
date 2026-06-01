//! Call-limiter seam — the b2bua side of the sliding-window limiter.
//!
//! The honest-outcome trait: [`CallLimiter::admit`] reports
//! [`AdmitOutcome::Admitted`] / [`AdmitOutcome::Rejected`] /
//! [`AdmitOutcome::Unavailable`], and the **call site owns the fail-open
//! policy** (`apply_route` maps `Unavailable` → admit with no holds). admit is
//! batched + transactional: every entry for a call increments together or not
//! at all.
//!
//! The HTTP client implementation lives in [`crate::limiter_http`]; this module
//! is just the trait + a no-op (used when `LIMITER_URL` is unset and in tests
//! that don't exercise limits).

use async_trait::async_trait;

/// One limiter entry to admit: an id and its concurrent-call cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// A recorded admission: an id and the window its increment landed in. The b2bua
/// stores these on the call and replays them on release / refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterHold {
    /// The limiter id.
    pub limiter_id: String,
    /// The window timestamp the increment landed in.
    pub window: i64,
}

/// The honest outcome of a transactional admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// Every entry was incremented atomically at this server-computed window.
    Admitted {
        /// The shared window all entries landed in.
        window: i64,
    },
    /// At least one entry was over cap; nothing was incremented.
    Rejected {
        /// The first id found over cap.
        limiter_id: String,
    },
    /// The backend was unreachable / slow / errored. The caller decides what to
    /// do (b2bua: fail open — admit with no holds).
    Unavailable,
}

/// Admission/release/refresh seam for per-call concurrency limits.
#[async_trait]
pub trait CallLimiter: Send + Sync {
    /// Try to admit one call carrying `entries` (all-or-none).
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome;
    /// Release the recorded holds (decrement each, best-effort).
    async fn release(&self, holds: &[LimiterHold]);
    /// Migrate each hold to the current window; returns the updated holds. On
    /// failure the holds are returned unchanged.
    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold>;
}

/// Always admits (with no holds); release/refresh are no-ops. Used when
/// `LIMITER_URL` is unset, preserving today's non-limiting behaviour.
#[derive(Clone, Default)]
pub struct NoopLimiter;

#[async_trait]
impl CallLimiter for NoopLimiter {
    async fn admit(&self, _entries: &[LimiterEntry]) -> AdmitOutcome {
        // Treat "no limiter configured" like a down backend: admit, no holds, so
        // nothing is ever released or refreshed.
        AdmitOutcome::Unavailable
    }
    async fn release(&self, _holds: &[LimiterHold]) {}
    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        holds.to_vec()
    }
}
