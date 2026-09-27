//! Call-limiter seam — the b2bua side of the limiter keyed by the call.
//!
//! Every request names the call by its limiter key (`Call::limiter.key`,
//! unique over time): [`CallLimiter::admit`] replaces the call's whole set on
//! the server, checked net of what the call already holds, and reports
//! [`AdmitOutcome::Admitted`] / [`AdmitOutcome::Rejected`] /
//! [`AdmitOutcome::Released`] / [`AdmitOutcome::Unavailable`] /
//! [`AdmitOutcome::NotSent`]; [`CallLimiter::release`] drops the set of every
//! call it names (idempotent per key, a no-op for a key the server holds
//! nothing for), and is reached only through the worker's release queue
//! ([`crate::limiter_release`]);
//! [`CallLimiter::refresh`] extends its lease, or re-registers the set the
//! call carries when the server no longer holds it. The **call site owns the
//! fail-open policy** ([`state_after_admit`]): a failed admit leaves the call
//! as it was, a sent one owes the call's release, and only a confirmed set
//! refreshes. [`CallLimiter::health`] is the limiter's health answer, which
//! the worker's circuit breaker ([`crate::limiter_breaker`]) probes.
//!
//! The HTTP client implementation lives in [`crate::limiter_http`]; this module
//! is the trait + a no-op (used when `LIMITER_URL` is unset and in tests that
//! don't exercise limits).

use std::sync::Arc;

use async_trait::async_trait;
use call::CallLimiterState;

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
    /// The request left and no usable answer came back (unreachable, slow,
    /// errored, a bad body): it may have landed. The caller decides what to
    /// do (b2bua: fail open).
    Unavailable,
    /// No request left (no limiter configured, or a local guard such as an
    /// open circuit breaker refused to send one): nothing can have landed.
    /// The caller fails open.
    NotSent,
}

/// The call's admission state after an admit of `ids` answered `outcome`,
/// from its state `prior`. Every answered or lost request owes the call's
/// release; a cap refusal leaves the call uncounted when the limiter dropped
/// its set (`release_on_refusal`, or a call that held none) and as it was
/// otherwise; a lost answer and an unsent request leave the call counted as
/// it was, refreshing whatever set the limiter holds for its key.
pub fn state_after_admit(
    prior: &CallLimiterState,
    outcome: &AdmitOutcome,
    release_on_refusal: bool,
    ids: Vec<String>,
) -> CallLimiterState {
    let key = prior.key.clone();
    match outcome {
        AdmitOutcome::Admitted => CallLimiterState::admitted(key, ids),
        AdmitOutcome::Rejected { .. } if release_on_refusal || !prior.counted => {
            CallLimiterState::unconfirmed(key)
        }
        AdmitOutcome::Rejected { .. } | AdmitOutcome::Unavailable => prior.with_release_owed(),
        AdmitOutcome::Released => CallLimiterState::unconfirmed(key),
        AdmitOutcome::NotSent => prior.clone(),
    }
}

/// The honest outcome of one refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The call's lease was extended.
    Extended,
    /// The server held no set for the call (lapsed, or the server restarted):
    /// it re-created the set from the ids sent, with no cap check.
    Reregistered,
    /// The server holds nothing for the call and re-creates nothing: the call
    /// was released within the last lease.
    Released,
    /// The server holds nothing for the call and re-creates nothing: an admit
    /// of the call's key dropped its set, and no admit since replaced it.
    Dropped,
    /// The backend was unreachable / slow / errored.
    Unavailable,
}

/// The honest outcome of one release request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseAnswer {
    /// The limiter answered: every key sent is released (or was already).
    Released,
    /// The request left and no usable answer came back: it may have landed.
    /// The caller sends the keys again; a release is idempotent per key.
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
    /// Drop the set of every call of `keys` in one request. Idempotent per
    /// key on the server. The implementation bounds the wait.
    async fn release(&self, keys: &[String]) -> ReleaseAnswer;
    /// Extend the call's lease; `ids` is the set the server re-registers
    /// when it no longer holds one for the call.
    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome;
    /// The limiter's health answer, which the worker's circuit breaker
    /// probes. A limiter without one runs without a breaker.
    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        None
    }
}

/// A limiter's health answer.
#[async_trait]
pub trait LimiterHealth: Send + Sync {
    /// Whether the limiter answered a request that reads its store, within
    /// the admit budget: an admit sent now can be served.
    async fn serving(&self) -> bool;
    /// Whether the limiter's address is known: a limiter whose name has not
    /// resolved has none, and a breaker guarding it starts open.
    fn has_address(&self) -> bool {
        true
    }
    /// Forget the limiter's address, so the next request looks its name up
    /// again. A limiter at a fixed address keeps it.
    fn forget_address(&self) {}
}

/// No limiter: every admit sends nothing ([`AdmitOutcome::NotSent`]), so no
/// call is counted, refreshed or owes a release. Used when `LIMITER_URL` is
/// unset, preserving the non-limiting behaviour.
#[derive(Clone, Default)]
pub struct NoopLimiter;

#[async_trait]
impl CallLimiter for NoopLimiter {
    async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
        AdmitOutcome::NotSent
    }
    async fn release(&self, _keys: &[String]) -> ReleaseAnswer {
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _key: &str, _ids: &[String]) -> RefreshOutcome {
        RefreshOutcome::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn counted() -> CallLimiterState {
        CallLimiterState::admitted("c#k".into(), ids(&["x"]))
    }

    fn fresh() -> CallLimiterState {
        CallLimiterState::uncounted("c#k".into())
    }

    fn refused() -> AdmitOutcome {
        AdmitOutcome::Rejected { limiter_id: "y".into() }
    }

    #[test]
    fn a_sent_initial_admit_owes_the_release_whatever_its_answer() {
        for outcome in [refused(), AdmitOutcome::Released, AdmitOutcome::Unavailable] {
            let state = state_after_admit(&fresh(), &outcome, false, ids(&["x", "y"]));
            assert_eq!(state, CallLimiterState::unconfirmed("c#k".into()), "{outcome:?}");
        }
        let state = state_after_admit(&fresh(), &AdmitOutcome::Admitted, false, ids(&["x", "y"]));
        assert_eq!(state, CallLimiterState::admitted("c#k".into(), ids(&["x", "y"])));
    }

    #[test]
    fn an_unsent_admit_leaves_the_call_as_it_was() {
        let state = state_after_admit(&fresh(), &AdmitOutcome::NotSent, false, ids(&["x"]));
        assert_eq!(state, fresh(), "an unsent initial admit owes nothing");
        let state = state_after_admit(&counted(), &AdmitOutcome::NotSent, true, ids(&["y"]));
        assert_eq!(state, counted(), "a counted call stays counted on its set");
    }

    #[test]
    fn a_lost_reroute_answer_keeps_the_call_counted_on_its_confirmed_ids() {
        let state = state_after_admit(&counted(), &AdmitOutcome::Unavailable, true, ids(&["y"]));
        assert_eq!(state, counted());
        assert!(state.release_owed);
    }

    #[test]
    fn a_refused_reroute_drops_the_set_only_when_it_asked_to() {
        let state = state_after_admit(&counted(), &refused(), true, ids(&["y"]));
        assert_eq!(state, CallLimiterState::unconfirmed("c#k".into()));
        let state = state_after_admit(&counted(), &refused(), false, ids(&["y"]));
        assert_eq!(state, counted(), "the old set stays");
        let state = state_after_admit(&counted(), &AdmitOutcome::Released, true, ids(&["y"]));
        assert_eq!(state, CallLimiterState::unconfirmed("c#k".into()));
    }
}
