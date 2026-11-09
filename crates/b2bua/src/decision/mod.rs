//! The call-decision adapter seam: the port, [`CallDecisionEngine`], and the
//! decision DTOs are `b2bua-decision`'s, re-exported here. The production HTTP
//! adapter and the deterministic `ScriptedDecisionEngine` (feature `testkit`)
//! both implement the port; the router reaches any of them through
//! [`DeadlineDecisionEngine`]. The consults that await an engine live in
//! [`apply_route`] and [`crate::initial_consult`].

pub mod apply_route;
mod limiter_failover;
#[cfg(any(test, feature = "testkit"))]
pub mod test_adapter;

pub use b2bua_decision::*;
#[cfg(any(test, feature = "testkit"))]
pub use test_adapter::{default_call_refer, ReferOutcome, ReleaseOutcome, ScriptedDecisionEngine};

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;

use crate::setup_budget::SetupBudget;

/// The one bound on a decision round trip (ADR-0022 X1): the core wraps
/// whatever engine the host injects (`B2buaCore::spawn_with_overload`), so the
/// bound holds however an adapter is written,
/// [`call_control_timeout`](crate::config::B2buaConfig::call_control_timeout)
/// long: no round trip runs unbounded.
///
/// A consult on a setup's turn is bounded by what is left of the setup's budget
/// ([`new_call_within`](Self::new_call_within),
/// [`call_failure_within`](Self::call_failure_within)): a caller behind the 100
/// waits for all of them together. A consult off the call's turn gets the whole
/// deadline per round trip (the `/call/failure` after a b-leg's failure, whose
/// chain ADR-0039's answer deadline bounds, and the release consult, which
/// nothing else bounds). `call_refer` passes straight through: the REFER has
/// its 202 and the subscription timers bound its lifecycle.
///
/// A round trip past its bound fails as [`CallDecisionError::Unavailable`], its
/// inner future dropped (for an HTTP adapter, the request cancelled). On a
/// setup's turn that answers the caller the bare 503, whichever consult of the
/// chain it cut; off the turn, the fold of an unanswered consult; on a release
/// consult, the local teardown.
pub struct DeadlineDecisionEngine {
    inner: std::sync::Arc<dyn CallDecisionEngine>,
    deadline: Duration,
    /// Deadline-breach aggregation keyed by decision method (ADR-0026): an
    /// unreachable backend is ONE episode per method — rising edge, ~5 s
    /// summaries, falling-edge totals — ended once answers have landed inside
    /// the deadline for the idle window.
    breaches: std::sync::Arc<observe::WaveSet>,
}

impl DeadlineDecisionEngine {
    /// Wrap `inner` with `deadline` (`B2buaConfig::call_control_timeout`).
    pub fn wrap(
        inner: std::sync::Arc<dyn CallDecisionEngine>,
        deadline: Duration,
    ) -> std::sync::Arc<Self> {
        let breaches = crate::lifecycle::backend_waves("decision-engine");
        std::sync::Arc::new(Self { inner, deadline, breaches })
    }

    /// The new-call round trip under what is left of `budget`.
    pub async fn new_call_within(
        &self,
        req: NewCallRequest,
        budget: SetupBudget,
    ) -> Result<NewCallResponse, CallDecisionError> {
        self.bounded("/call/new", self.within(budget), self.inner.new_call(req)).await
    }

    /// The limiter-refusal failure consult under what is left of `budget`.
    pub async fn call_failure_within(
        &self,
        req: CallFailureRequest,
        budget: SetupBudget,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.bounded("/call/failure", self.within(budget), self.inner.call_failure(req)).await
    }

    /// What is left of `budget`; the whole deadline for a turn that is no
    /// setup ([`SetupBudget::NONE`]).
    fn within(&self, budget: SetupBudget) -> Duration {
        budget.remaining().unwrap_or(self.deadline)
    }

    /// Run `ask` up to `bound`: nothing is asked of a bound already spent.
    async fn bounded<T>(
        &self,
        method: &'static str,
        bound: Duration,
        ask: impl Future<Output = Result<T, CallDecisionError>>,
    ) -> Result<T, CallDecisionError> {
        if bound.is_zero() {
            return Err(expired(method, bound));
        }
        self.observe_outcome(method, bound, tokio::time::timeout(bound, ask).await)
    }

    /// Fold one round-trip outcome into `method`'s degradation episode: a
    /// deadline breach and any engine error — `Unavailable`, and a `Refused`
    /// the engine stated because it read no decision — belong to the SAME
    /// episode (both are "the decision engine is not answering calls"); real
    /// answers for the idle window end it. Logging only; aggregated, never one
    /// line per call (ADR-0026).
    fn observe_outcome<T>(
        &self,
        method: &'static str,
        bound: Duration,
        outcome: Result<Result<T, CallDecisionError>, tokio::time::error::Elapsed>,
    ) -> Result<T, CallDecisionError> {
        match outcome {
            Ok(Ok(v)) => {
                if self.breaches.is_active() {
                    self.breaches.recovered(method);
                }
                Ok(v)
            }
            Ok(Err(e)) => {
                self.breaches.record(method, "unavailable", 1);
                Err(e)
            }
            Err(_) => {
                self.breaches.record(method, "deadline_breaches", 1);
                Err(expired(method, bound))
            }
        }
    }
}

/// The failure of a `method` round trip past its `bound`.
fn expired(method: &str, bound: Duration) -> CallDecisionError {
    CallDecisionError::Unavailable(format!(
        "decision deadline: no {method} answer within {}ms",
        bound.as_millis()
    ))
}

#[async_trait]
impl CallDecisionEngine for DeadlineDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.bounded("/call/new", self.deadline, self.inner.new_call(req)).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.bounded("/call/failure", self.deadline, self.inner.call_failure(req)).await
    }
    /// Passes straight through: see the type doc.
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
    /// Bounded: not caller-blocking, but nothing else bounds it, and a hung
    /// consult must not wedge the established call.
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.bounded("/call/release", self.deadline, self.inner.call_release(req)).await
    }
}
