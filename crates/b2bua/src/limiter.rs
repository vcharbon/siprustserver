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
//! [`CallLimiter::refresh`] extends the lease of every call it names, or
//! re-registers the set a call carries when the server no longer holds it,
//! and is reached only through the worker's refresh batch
//! ([`crate::limiter_refresh_batch`]). The **call site owns the
//! fail-open policy** ([`state_after_admit`]): a failed admit leaves the call
//! as it was, a sent one owes the call's release, and only a confirmed set
//! refreshes. [`CallLimiter::health`] is the limiter's health answer, which
//! the worker's circuit breaker ([`crate::limiter_breaker`]) probes;
//! [`CallLimiter::report_to`] registers where a client reports what the
//! limiter's answers say beyond each outcome ([`LimiterReports`]): the lease
//! they state, and the answers it could not read.
//!
//! The HTTP client implementation lives in [`crate::limiter_http`]; this module
//! is the trait + a no-op (used when `LIMITER_URL` is unset and in tests that
//! don't exercise limits).

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use call::CallLimiterState;

use crate::limiter_lease::LimiterLease;
use crate::metrics::{B2buaMetrics, LimiterFailure, LimiterOp};

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
/// it was, refreshing whatever set the limiter holds for its key. A lost
/// answer, an unsent request and a release fence leave an uncounted call
/// `fail_open` when `ids` is not empty.
pub fn state_after_admit(
    prior: &CallLimiterState,
    outcome: &AdmitOutcome,
    release_on_refusal: bool,
    ids: Vec<String>,
) -> CallLimiterState {
    let key = prior.key.clone();
    let wanted = !ids.is_empty();
    match outcome {
        AdmitOutcome::Admitted => CallLimiterState::admitted(key, ids),
        AdmitOutcome::Rejected { .. } if release_on_refusal || !prior.counted => {
            CallLimiterState::unconfirmed(key)
        }
        AdmitOutcome::Rejected { .. } => prior.with_release_owed(),
        AdmitOutcome::Unavailable => prior.with_release_owed().failed_open(wanted),
        AdmitOutcome::Released => CallLimiterState::unconfirmed(key).failed_open(wanted),
        AdmitOutcome::NotSent => prior.failed_open(wanted),
    }
}

/// One call a refresh names: its key and the ids the server re-registers
/// when it no longer holds a set for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshCall {
    /// The call's limiter key.
    pub key: String,
    /// The ids the server last confirmed for the call.
    pub ids: Vec<String>,
}

/// The server's answer for one call a refresh named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// The honest outcome of one refresh request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshAnswer {
    /// The limiter answered: one outcome per call named, in order.
    Answered(Vec<RefreshOutcome>),
    /// The request left and no usable answer came back (unreachable, slow,
    /// errored, a bad body): it may have landed. The caller sends the calls
    /// again; a refresh is idempotent per call.
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
    /// Extend the lease of every call of `calls` in one request; a call's
    /// `ids` is the set the server re-registers when it no longer holds one
    /// for it. The implementation bounds the wait.
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer;
    /// The limiter's health answer, which the worker's circuit breaker
    /// probes. A limiter without one runs without a breaker.
    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        None
    }
    /// From now on, report to `reports` what the limiter's answers say: a
    /// limiter reached over a wire reports every lease stated, every request
    /// sent and every request that got no usable answer; a wrapper forwards
    /// to what it wraps; a limiter with neither reports nothing.
    fn report_to(&self, reports: LimiterReports);
}

/// Where a limiter client reports what the limiter's answers say beyond each
/// request's outcome: the lease they state, to the worker's [`LimiterLease`],
/// and every request it sent and each one that got no usable answer, by
/// cause, on the worker's metrics. It holds the
/// lease weakly: once the worker is gone its reports go nowhere and the
/// client drops the registration.
#[derive(Clone)]
pub struct LimiterReports {
    lease: Weak<LimiterLease>,
    metrics: B2buaMetrics,
}

impl LimiterReports {
    /// Reports to `lease` and `metrics`.
    pub fn new(lease: &Arc<LimiterLease>, metrics: B2buaMetrics) -> Self {
        Self { lease: Arc::downgrade(lease), metrics }
    }

    /// Whether the worker the reports go to is still there.
    pub fn is_live(&self) -> bool {
        self.lease.strong_count() > 0
    }

    /// An answer stated the limiter's lease.
    pub fn lease_stated(&self, lease: Duration) {
        if let Some(to) = self.lease.upgrade() {
            to.learn(lease);
        }
    }

    /// A request of `op` left for the limiter.
    pub fn sent(&self, op: LimiterOp) {
        if self.is_live() {
            self.metrics.limiter().count_request(op);
        }
    }

    /// A request of `op` got no usable answer for `cause`.
    pub fn failed(&self, op: LimiterOp, cause: LimiterFailure) {
        if self.is_live() {
            self.metrics.limiter().count_failure(op, cause);
        }
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
    async fn refresh(&self, _calls: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: LimiterReports) {}
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

    /// The unconfirmed state of an uncounted call whose route names ids.
    fn failing_open() -> CallLimiterState {
        CallLimiterState { fail_open: true, ..CallLimiterState::unconfirmed("c#k".into()) }
    }

    #[test]
    fn a_sent_initial_admit_owes_the_release_whatever_its_answer() {
        let state = state_after_admit(&fresh(), &refused(), false, ids(&["x", "y"]));
        assert_eq!(state, CallLimiterState::unconfirmed("c#k".into()), "a cap refusal");
        for outcome in [AdmitOutcome::Released, AdmitOutcome::Unavailable] {
            let state = state_after_admit(&fresh(), &outcome, false, ids(&["x", "y"]));
            assert_eq!(state, failing_open(), "{outcome:?}");
        }
        let state = state_after_admit(&fresh(), &AdmitOutcome::Admitted, false, ids(&["x", "y"]));
        assert_eq!(state, CallLimiterState::admitted("c#k".into(), ids(&["x", "y"])));
    }

    #[test]
    fn an_unsent_admit_leaves_the_call_as_it_was() {
        let state = state_after_admit(&fresh(), &AdmitOutcome::NotSent, false, ids(&["x"]));
        assert_eq!(
            state,
            CallLimiterState { fail_open: true, ..fresh() },
            "an unsent initial admit owes nothing and runs uncounted"
        );
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
        assert_eq!(state, failing_open(), "the call was released under it");
    }

    #[test]
    fn a_failed_admit_runs_uncounted_only_when_its_route_names_ids() {
        for outcome in [AdmitOutcome::Unavailable, AdmitOutcome::NotSent, AdmitOutcome::Released] {
            let state = state_after_admit(&fresh(), &outcome, true, Vec::new());
            assert!(!state.fail_open, "{outcome:?} of an empty route asks for nothing");
            let state = state_after_admit(&failing_open(), &outcome, true, Vec::new());
            assert!(!state.fail_open, "{outcome:?}: the new route asks for nothing");
            let state = state_after_admit(&failing_open(), &outcome, true, ids(&["y"]));
            assert!(state.fail_open, "{outcome:?}: still uncounted");
        }
        let state = state_after_admit(&failing_open(), &AdmitOutcome::Admitted, true, ids(&["y"]));
        assert!(state.counted && !state.fail_open, "an admitted route counts the call");
        let state = state_after_admit(&failing_open(), &refused(), true, ids(&["y"]));
        assert!(!state.fail_open, "a cap refusal ends the fail-open");
    }
}
