//! Call-limiter seam — the b2bua side of the limiter keyed by the call.
//!
//! Every request names the call by its limiter key (`Call::limiter.key()`, unique
//! over time): [`CallLimiter::admit`] replaces the call's whole set on the
//! server under the call's next change number, checked net of what the call
//! already holds (it carries the call's held set, which a server that lost the
//! call's set re-registers first), and reports an [`AdmitOutcome`];
//! [`CallLimiter::release`] drops the set of every call it names (idempotent
//! per key, a no-op for a key the server holds nothing for), and is reached
//! only through the worker's release queue ([`crate::limiter::release_queue`]);
//! [`CallLimiter::refresh`] extends the lease of every call it names, or
//! re-registers the set a call carries when the server no longer holds it, and
//! is reached only through the worker's refresh batch
//! ([`crate::limiter::refresh_batch`]). Every admit and refresh answer states
//! the set the server holds for the key, which the call applies as its held set
//! ([`call::CallLimiterState`]). The **call site owns the fail-open policy**: a
//! failed admit leaves held as it was, a sent one owes the call's release, and
//! only a confirmed set refreshes. [`CallLimiter::health`] is the limiter's
//! health answer, which the worker's circuit breaker
//! ([`crate::limiter::breaker`]) probes; [`CallLimiter::report_to`] registers
//! where a client reports what the limiter's answers say beyond each outcome
//! ([`LimiterReports`]): the lease they state, and the answers it could not
//! read.
//!
//! The HTTP client implementation lives in [`crate::limiter::http`]; this module
//! is the trait + a no-op (used when `LIMITER_URL` is unset and in tests that
//! don't exercise limits).

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
pub use call::{AdmitOutcome, LimiterEntry, LimiterHeld};

use crate::limiter::lease::LimiterLease;
use crate::metrics::{B2buaMetrics, LimiterFailure, LimiterOp};

/// One call a refresh names: its key and the set the server re-registers
/// when it no longer holds one for it: the call's held set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshCall {
    /// The call's limiter key.
    pub key: String,
    /// The set the server last stated it holds for the call.
    pub held: LimiterHeld,
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

/// The server's answer for one call a refresh named: its outcome and, unless
/// the call was released, the set the server holds for the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshReply {
    /// What the refresh did.
    pub outcome: RefreshOutcome,
    /// The set the server holds for the key (`None` iff released).
    pub held: Option<LimiterHeld>,
}

impl RefreshReply {
    /// A reply of `outcome` for `call` stating the set the call carried: what
    /// a limiter answers when it holds what the call sent (none when
    /// released, an empty set when dropped).
    pub fn restating(call: &RefreshCall, outcome: RefreshOutcome) -> Self {
        let held = match outcome {
            RefreshOutcome::Released => None,
            RefreshOutcome::Dropped => {
                Some(LimiterHeld { change: call.held.change, entries: vec![] })
            }
            RefreshOutcome::Extended | RefreshOutcome::Reregistered => Some(call.held.clone()),
        };
        Self { outcome, held }
    }
}

/// The honest outcome of one refresh request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshAnswer {
    /// The limiter answered: one reply per call named, in order.
    Answered(Vec<RefreshReply>),
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

/// The admit budget of a limiter answering without a wire (a no-op, a
/// test double).
pub const LOCAL_ADMIT_BUDGET: Duration = Duration::from_millis(150);

/// Admission/release/refresh seam for per-call concurrency limits.
#[async_trait]
pub trait CallLimiter: Send + Sync {
    /// Replace the call's set with `entries` under the call's `change` number
    /// (all-or-none, net of the set the call holds; superseded when not above
    /// the number of the set held). `held` is the call's held set, which a
    /// server that knows nothing of the key re-registers before the check.
    /// `release_on_refusal` drops the call's current set in the same step
    /// when a cap refuses.
    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome;
    /// Drop the set of every call of `keys` in one request. Idempotent per
    /// key on the server. The implementation bounds the wait.
    async fn release(&self, keys: &[String]) -> ReleaseAnswer;
    /// Extend the lease of every call of `calls` in one request; a call's
    /// `held` is the set the server re-registers when it no longer holds one
    /// for it. The implementation bounds the wait.
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer;
    /// The longest an admit waits for its answer; past it the admit is
    /// [`AdmitOutcome::Unavailable`]. The worker bounds every admit just past
    /// it ([`crate::limiter::bounded`]); a service admit's answer deadline is
    /// this budget plus a margin, and a `/call/failure` consult's counts it
    /// once per round of its chain (ADR-0039). A wrapper states the budget of
    /// what it wraps.
    fn admit_budget(&self) -> Duration;
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
    pub(crate) fn is_live(&self) -> bool {
        self.lease.strong_count() > 0
    }

    /// An answer stated the limiter's lease.
    pub(crate) fn lease_stated(&self, lease: Duration) {
        if let Some(to) = self.lease.upgrade() {
            to.learn(lease);
        }
    }

    /// A request of `op` left for the limiter.
    pub(crate) fn sent(&self, op: LimiterOp) {
        if self.is_live() {
            self.metrics.limiter().count_request(op);
        }
    }

    /// A request of `op` got no usable answer for `cause`.
    pub(crate) fn failed(&self, op: LimiterOp, cause: LimiterFailure) {
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
    fn admit_budget(&self) -> std::time::Duration {
        crate::limiter::LOCAL_ADMIT_BUDGET
    }

    async fn admit(
        &self,
        _: &str,
        _: u64,
        _: &LimiterHeld,
        _: &[LimiterEntry],
        _: bool,
    ) -> AdmitOutcome {
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

/// Unit-test shorthands for the limiter seam's sets and replies.
#[cfg(test)]
pub(crate) mod testkit {
    use super::{LimiterEntry, LimiterHeld, RefreshCall, RefreshOutcome, RefreshReply};

    /// `ids` at cap 10, stated under change 1.
    pub(crate) fn held_of(ids: &[String]) -> LimiterHeld {
        let entries = ids.iter().map(|id| LimiterEntry { id: id.clone(), limit: 10 }).collect();
        LimiterHeld { change: 1, entries }
    }

    /// One `outcome` per call ([`RefreshReply::restating`]).
    pub(crate) fn replies(calls: &[RefreshCall], outcome: RefreshOutcome) -> Vec<RefreshReply> {
        calls.iter().map(|c| RefreshReply::restating(c, outcome)).collect()
    }
}
