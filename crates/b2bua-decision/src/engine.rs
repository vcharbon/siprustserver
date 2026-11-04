//! [`CallDecisionEngine`]: the backend that decides how to handle a call.

use async_trait::async_trait;
use b2bua_core::decision::{
    CallDecisionError, CallFailureRequest, CallFailureResponse, CallReferRequest,
    CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest, NewCallResponse,
};

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
    /// Decide release vs reroute when a **subscribed** internal release event
    /// fires (max-call-duration first). Consulted only when
    /// the event kind is in `Call.subscriptions` AND the call carries a
    /// `callback_context` — otherwise the core acts locally and this is never
    /// called.
    ///
    /// **Defaulted for back-compat**: existing engines compile unchanged and
    /// behave exactly as before (every release event → local teardown); an
    /// engine opts into the release round-trip by overriding this.
    async fn call_release(
        &self,
        _req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        Ok(CallReleaseResponse::Release { label: None, service_ext: Default::default() })
    }
}
