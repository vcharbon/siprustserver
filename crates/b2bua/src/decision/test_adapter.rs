//! `ScriptedDecisionEngine` — a deterministic, no-HTTP decision backend that
//! emulates the jssip reference backend by inspecting the request JSON
//! (R-URI / To / `X-*` headers / body). Tests configure it with predicates so
//! the existing SIPp scenario scripts produce the same call flows; the default
//! routes every call to a configured destination with mandatory platform
//! features.

use async_trait::async_trait;

use super::schemas::{
    default_platform_features, BodyUpdate, NewCallRequest, NewCallResponse, RejectDecision,
    RouteDecision, SipDestination,
};
use super::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse,
};

type NewCallRule = Box<dyn Fn(&NewCallRequest) -> Option<NewCallResponse> + Send + Sync>;

/// A scripted decision backend. Build with [`ScriptedDecisionEngine::route_all_to`]
/// for the common case, or [`ScriptedDecisionEngine::builder`] for payload-driven
/// rules.
pub struct ScriptedDecisionEngine {
    rules: Vec<NewCallRule>,
    fallback: NewCallFallback,
    failure: FailureRule,
}

impl ScriptedDecisionEngine {
    /// The common case: route every call to `host:port` with default platform
    /// features and terminate on b-leg failure.
    pub fn route_all_to(host: impl Into<String>, port: u16) -> Self {
        let dest = (host.into(), port);
        Self::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to(&dest.0, dest.1))
            })
            .build()
    }

    pub fn builder() -> ScriptedBuilder {
        ScriptedBuilder {
            rules: Vec::new(),
            fallback: None,
            failure: None,
        }
    }
}

/// Build a [`RouteDecision`] to `host:port` with default platform features.
pub fn route_to(host: &str, port: u16) -> RouteDecision {
    RouteDecision {
        destination: SipDestination::new(host, port),
        new_ruri: None,
        update_headers: None,
        update_body: BodyUpdate::Keep,
        no_answer_timeout_sec: None,
        call_limiter: Vec::new(),
        callback_context: None,
        features: default_platform_features(),
        service_ext: Default::default(),
    }
}

/// A reject decision.
pub fn reject(code: u16, reason: impl Into<String>) -> NewCallResponse {
    NewCallResponse::Reject(RejectDecision {
        reject_code: code,
        reject_reason: Some(reason.into()),
        update_headers: None,
    })
}

type NewCallFallback = Box<dyn Fn(&NewCallRequest) -> NewCallResponse + Send + Sync>;
type FailureRule = Box<dyn Fn(&CallFailureRequest) -> CallFailureResponse + Send + Sync>;

pub struct ScriptedBuilder {
    rules: Vec<NewCallRule>,
    fallback: Option<NewCallFallback>,
    failure: Option<FailureRule>,
}

impl ScriptedBuilder {
    /// Add a payload predicate → response rule (first match wins).
    pub fn on(
        mut self,
        rule: impl Fn(&NewCallRequest) -> Option<NewCallResponse> + Send + Sync + 'static,
    ) -> Self {
        self.rules.push(Box::new(rule));
        self
    }

    pub fn fallback(
        mut self,
        f: impl Fn(&NewCallRequest) -> NewCallResponse + Send + Sync + 'static,
    ) -> Self {
        self.fallback = Some(Box::new(f));
        self
    }

    pub fn on_failure(
        mut self,
        f: impl Fn(&CallFailureRequest) -> CallFailureResponse + Send + Sync + 'static,
    ) -> Self {
        self.failure = Some(Box::new(f));
        self
    }

    pub fn build(self) -> ScriptedDecisionEngine {
        ScriptedDecisionEngine {
            rules: self.rules,
            fallback: self
                .fallback
                .unwrap_or_else(|| Box::new(|_| reject(404, "Not Found"))),
            failure: self.failure.unwrap_or_else(|| Box::new(|_| CallFailureResponse::Terminate)),
        }
    }
}

#[async_trait]
impl CallDecisionEngine for ScriptedDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        for rule in &self.rules {
            if let Some(resp) = rule(&req) {
                return Ok(resp);
            }
        }
        Ok((self.fallback)(&req))
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Ok((self.failure)(&req))
    }

    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        // Transfer is a deferred slice — deny by default.
        Ok(CallReferResponse::Reject {
            code: 501,
            reason: Some("Not Implemented".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(to_user: &str) -> NewCallRequest {
        NewCallRequest {
            call_id: "cid".into(),
            ruri: format!("sip:{to_user}@example.com"),
            to: format!("<sip:{to_user}@example.com>"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn route_all_to_routes_every_call() {
        let eng = ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070);
        match eng.new_call(req("bob")).await.unwrap() {
            NewCallResponse::Route(r) => {
                assert_eq!(r.destination.host, "127.0.0.1");
                assert_eq!(r.destination.port(), 5070);
                assert_eq!(r.features.platform.max_duration_sec, 3_600);
            }
            _ => panic!("expected route"),
        }
    }

    #[tokio::test]
    async fn payload_rule_can_reject() {
        let eng = ScriptedDecisionEngine::builder()
            .on(|r| r.to.contains("blocked").then(|| reject(403, "Forbidden")))
            .fallback(|_| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build();
        match eng.new_call(req("blocked")).await.unwrap() {
            NewCallResponse::Reject(rj) => assert_eq!(rj.reject_code, 403),
            _ => panic!("expected reject"),
        }
        match eng.new_call(req("bob")).await.unwrap() {
            NewCallResponse::Route(_) => {}
            _ => panic!("expected route"),
        }
    }
}
