//! The call-decision adapter seam — port of `CallDecisionEngine`. The B2BUA
//! core depends only on this trait; the production HTTP adapter and the
//! deterministic [`ScriptedDecisionEngine`] both implement it. `apply_route` /
//! `handle_initial_invite` (the response→call-state translation) live in
//! [`apply_route`] / [`crate::initial_invite`].

pub mod apply_route;
mod schemas;
pub mod test_adapter;

pub use schemas::{
    default_platform_features, BodyUpdate, CallFailureRequest, CallFailureResponse, CallLimiterEntry,
    CallReferRequest, CallReferResponse, CallSnapshot, CallTreatment, FailureInfo,
    FeatureActivations, LegSnapshot, NewCallRequest, NewCallResponse, RedirectContact,
    RedirectDecision, RejectDecision, RouteDecision, SipDestination, SipHeaderUpdates,
};
pub use test_adapter::{default_call_refer, ReferOutcome, ScriptedDecisionEngine};

use async_trait::async_trait;

/// The backend that decides how to handle a call. In production it is an HTTP
/// client; in tests it is the scripted adapter.
#[async_trait]
pub trait CallDecisionEngine: Send + Sync {
    /// Decide routing/services for a new INVITE.
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError>;
    /// Decide failover vs terminate when a b-leg fails.
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError>;
    /// Authorize/deny a REFER transfer (dormant until the transfer slice).
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CallDecisionError {
    #[error("decision backend unavailable: {0}")]
    Unavailable(String),
}

/// Hard per-round-trip deadline on the decision backend (ADR-0022). The core
/// wraps whatever engine the host injects (see `B2buaCore::spawn_with_overload`)
/// so the initial-INVITE guarantee — *a caller who heard the auto-100 gets a
/// final response within the decision deadline, not a wedged await* — holds
/// **structurally**, regardless of how a (possibly third-party) adapter is
/// written. The TS system enforced this inside its `HttpReferenceAdapter`
/// (`callControlNewCallTimeoutMs` / `callControlFailureTimeoutMs`); moving it
/// here means an adapter that forgets its own timeout can no longer strand a
/// call.
///
/// **Scope — `new_call` + `call_failure` only, deliberately NOT `call_refer`**
/// (ADR-0022). These two are the decision calls that can block a caller who is
/// waiting behind the INVITE's auto-100: `new_call` for the initial route, and
/// `call_failure` for the limiter-reject / no-answer failover that reroutes
/// *toward a still-pending INVITE final*. `call_refer` is different in kind —
/// the REFER already received its `202 Accepted`, so a hanging refer
/// authorization strands no waiting INVITE; it is bounded instead by the
/// dedicated `refer_subscription_expiry_sec` (60 s) + `refer_overall_safety_sec`
/// (120 s) timers (see `refer_reject.rs::refer_http_timeout`). This is a
/// documented divergence from the TS `callControlReferTimeoutMs`: the Rust port
/// bounds the REFER lifecycle with those subscription timers rather than a
/// decision deadline, so `call_refer` passes straight through.
///
/// On expiry the wrapped methods fail with [`CallDecisionError::Unavailable`],
/// which every call-site already handles: `new_call` → 503 to the caller
/// (`handle_initial_invite`), limiter-failover `call_failure` → 486 and async
/// b-leg `call_failure` → its existing `Err` fallback (`apply_route` /
/// `no-answer`). Rides `tokio::time::timeout` (deterministic under
/// `start_paused` tests). The expired inner future is dropped — for an HTTP
/// adapter that cancels the in-flight request, exactly like the TS
/// `Effect.timeoutOrElse`.
pub struct DeadlineDecisionEngine {
    inner: std::sync::Arc<dyn CallDecisionEngine>,
    deadline: std::time::Duration,
}

impl DeadlineDecisionEngine {
    /// Wrap `inner` with `timeout_ms` (from `B2buaConfig::call_control_timeout_ms`).
    /// `<= 0` disables: returns `inner` unwrapped (the escape hatch the reaper
    /// wedge tests use to exercise the abort-escalation ladder).
    pub fn wrap(
        inner: std::sync::Arc<dyn CallDecisionEngine>,
        timeout_ms: i64,
    ) -> std::sync::Arc<dyn CallDecisionEngine> {
        if timeout_ms <= 0 {
            return inner;
        }
        std::sync::Arc::new(Self {
            inner,
            deadline: std::time::Duration::from_millis(timeout_ms as u64),
        })
    }

    fn expired(&self, method: &str) -> CallDecisionError {
        CallDecisionError::Unavailable(format!(
            "decision deadline: no {method} answer within {}ms",
            self.deadline.as_millis()
        ))
    }
}

#[async_trait]
impl CallDecisionEngine for DeadlineDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        match tokio::time::timeout(self.deadline, self.inner.new_call(req)).await {
            Ok(r) => r,
            Err(_) => Err(self.expired("/call/new")),
        }
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        match tokio::time::timeout(self.deadline, self.inner.call_failure(req)).await {
            Ok(r) => r,
            Err(_) => Err(self.expired("/call/failure")),
        }
    }
    /// NOT deadline-wrapped — see the type doc. The REFER's 202 is already out;
    /// the refer subscription-expiry / overall-safety timers bound this.
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}
