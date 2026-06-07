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
    CallReferRequest, CallReferResponse, CallTreatment, FailureInfo, FeatureActivations,
    NewCallRequest, NewCallResponse, RedirectContact, RedirectDecision, RejectDecision,
    RouteDecision, SipDestination, SipHeaderUpdates,
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
