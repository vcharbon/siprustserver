//! Call-limiter seam — the b2bua side of the limiter keyed by the call.
//!
//! Every request names the call by its limiter key (`Call::limiter.key`,
//! unique over time): [`CallLimiter::admit`] replaces the call's whole set on
//! the server, checked net of what the call already holds, and reports
//! [`AdmitOutcome::Admitted`] / [`AdmitOutcome::Rejected`] /
//! [`AdmitOutcome::Released`] / [`AdmitOutcome::Unavailable`];
//! [`CallLimiter::release`] drops the set (idempotent);
//! [`CallLimiter::refresh`] extends its lease, or re-registers the set the
//! call carries when the server no longer holds it. The **call site owns the
//! fail-open policy**: an `Unavailable` admit leaves the call uncounted, and
//! an uncounted call never refreshes or releases.
//!
//! The HTTP client implementation lives in [`crate::limiter_http`]; this module
//! is the trait + a no-op (used when `LIMITER_URL` is unset and in tests that
//! don't exercise limits).

use async_trait::async_trait;

/// One limiter entry to admit: an id and its concurrent-call cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// The honest outcome of one admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// The call's set is now the entries sent; the call is counted.
    Admitted,
    /// An id the call adds is at its cap; the call's set is what it was, or
    /// nothing when the admit asked for release on refusal.
    Rejected {
        /// The first id found at its cap.
        limiter_id: String,
    },
    /// The call was released within the last lease: the server holds nothing
    /// for it and re-creates nothing.
    Released,
    /// The backend was unreachable / slow / errored. The caller decides what
    /// to do (b2bua: fail open — the call runs uncounted).
    Unavailable,
}

/// The honest outcome of one refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The call's lease was extended.
    Known,
    /// The server held no set for the call (lapsed, or the server restarted):
    /// it re-created the set from the ids sent, with no cap check.
    Reregistered,
    /// The call was released within the last lease: the server holds nothing
    /// for it and re-creates nothing.
    Released,
    /// The backend was unreachable / slow / errored.
    Unavailable,
}

/// Admission/release/refresh seam for per-call concurrency limits.
#[async_trait]
pub trait CallLimiter: Send + Sync {
    /// Replace the call's set with `entries` (all-or-none, net of the set the
    /// call holds). `release_on_refusal` drops the call's current set in the
    /// same step when a cap refuses.
    async fn admit(
        &self,
        key: &str,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome;
    /// Drop the call's set, best-effort. Idempotent on the server.
    async fn release(&self, key: &str);
    /// Extend the call's lease; `ids` is the set the server re-registers
    /// when it no longer holds one for the call.
    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome;
}

/// Always unavailable: every admit fails open, so nothing is ever released or
/// refreshed. Used when `LIMITER_URL` is unset, preserving the non-limiting
/// behaviour.
#[derive(Clone, Default)]
pub struct NoopLimiter;

#[async_trait]
impl CallLimiter for NoopLimiter {
    async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
        AdmitOutcome::Unavailable
    }
    async fn release(&self, _key: &str) {}
    async fn refresh(&self, _key: &str, _ids: &[String]) -> RefreshOutcome {
        RefreshOutcome::Unavailable
    }
}
