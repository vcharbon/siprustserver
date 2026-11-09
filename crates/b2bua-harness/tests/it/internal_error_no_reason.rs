//! An initial INVITE's internal error carries no `Reason` (ADR-0022 X3), whatever
//! the route that was applied before it stated. A route's stated `Reason` speaks
//! for the finals its decision answers for; the stack's own 503 answers for none,
//! and a `Reason` on it reads as the cause of a rejection nobody decided (RFC 3326).
//! The route's other stated headers ride as ever, and a backend's own reject keeps
//! the `Reason` it states.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallTreatment, NewCallRequest, NewCallResponse,
    RejectDecision, RouteDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{fail_open, never_answer_admit, refuse_on};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::features::StatedHeaders;
use call::header_update::HeaderUpdate;
use scenario_harness::callflow::OFFER_SDP;
use sip_message::header::HeaderName;

const STATED_REASON: &str = "Q.850;cause=34";
const STATED_OTHER: &str = "stated";

/// A route to bob stating a `Reason` and another header on the caller's finals.
fn stating_route(bob_port: u16, limiter: Option<&str>, context: bool) -> RouteDecision {
    let mut route = route_to("127.0.0.1", bob_port);
    route.callback_context = context.then(|| "ctx".to_string());
    route.call_limiter =
        limiter.map(|id| vec![LimiterEntry { id: id.into(), limit: 10 }]).unwrap_or_default();
    let mut stated = StatedHeaders::default();
    let lines = |line: &str| HeaderUpdate::Set(vec![line.to_string()]);
    stated.originator_finals.insert("Reason".into(), lines(STATED_REASON));
    stated.originator_finals.insert("X-Final".into(), lines(STATED_OTHER));
    route.features.stated_headers = Some(stated);
    route
}

/// The stack's own bare 503, the route's other statement on it.
fn assert_bare_503(resp: &sip_message::SipResponse) {
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.reason(), "Service Unavailable");
    assert!(!resp.has(&HeaderName::Reason), "no Reason: {resp:?}");
    assert!(!resp.has(&HeaderName::RetryAfter), "no Retry-After: {resp:?}");
    let other: Vec<_> = resp.raw(HeaderName::from("X-Final")).collect();
    assert_eq!(other, [STATED_OTHER], "the route's other statement rides");
}

/// Routes every call as `route`; its failure consult answers `failure`.
struct Backend {
    route: RouteDecision,
    failure: Result<CallTreatment, String>,
    answer_after: Duration,
}

#[async_trait]
impl CallDecisionEngine for Backend {
    async fn new_call(&self, _: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(self.answer_after).await;
        Ok(NewCallResponse::Route(self.route.clone()))
    }
    async fn call_failure(
        &self,
        _: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.failure.clone().map_err(CallDecisionError::Unavailable)
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        ScriptedDecisionEngine::builder().build().call_refer(req).await
    }
}

fn backend(route: RouteDecision, failure: Result<CallTreatment, String>) -> Arc<Backend> {
    Arc::new(Backend { route, failure, answer_after: Duration::ZERO })
}

#[tokio::test(start_paused = true)]
async fn a_route_refused_with_no_way_to_fail_over() {
    let s = B2buaScene::with_b2bua("bare-503-no-failover", |bob_port| {
        let route = stating_route(bob_port, Some("cap"), false);
        B2buaSut::builder(backend(route, Err("unused".into())))
            .limiter(refuse_on("cap", fail_open()))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    assert_bare_503(&call.expect(503).await);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_route_the_setup_budget_ran_out_on() {
    let s = B2buaScene::with_b2bua("bare-503-budget", |bob_port| {
        let route = stating_route(bob_port, Some("trunk"), true);
        B2buaSut::builder(backend(route, Err("unused".into())))
            .limiter(never_answer_admit(Duration::from_secs(8), fail_open()))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    call.expect(100).await;
    s.h.advance(Duration::from_secs(5)).await;
    assert_bare_503(&call.expect(503).await);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}

/// On the turn: the consult a limiter refusal raises goes unanswered.
#[tokio::test(start_paused = true)]
async fn an_unanswered_consult_on_the_turn() {
    let s = B2buaScene::with_b2bua("bare-503-consult-on-turn", |bob_port| {
        let route = stating_route(bob_port, Some("cap"), true);
        B2buaSut::builder(backend(route, Err("down".into()))).limiter(refuse_on("cap", fail_open()))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    assert_bare_503(&call.expect(503).await);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}

/// Off the turn: the consult a callee's failure raises goes unanswered.
#[tokio::test(start_paused = true)]
async fn an_unanswered_consult_off_the_turn() {
    let s = B2buaScene::with_b2bua("bare-503-consult-off-turn", |bob_port| {
        let route = stating_route(bob_port, None, true);
        B2buaSut::builder(backend(route, Err("down".into())))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    assert_bare_503(&call.expect(503).await);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}

/// A backend's reject is its own statement: a 503 with its `Reason` keeps it, a
/// decision and not an internal error.
#[tokio::test(start_paused = true)]
async fn a_backends_own_reject_keeps_the_reason_it_states() {
    const ITS_OWN: &str = "Q.850;cause=21";
    let s = B2buaScene::with_b2bua("bare-503-backend-reject", |bob_port| {
        let route = stating_route(bob_port, Some("cap"), true);
        let mut updates = call::header_update::SipHeaderUpdates::default();
        updates.insert("Reason".into(), HeaderUpdate::Set(vec![ITS_OWN.to_string()]));
        let reject = RejectDecision {
            reject_code: 503,
            reject_reason: Some("Service Unavailable".into()),
            update_headers: Some(updates),
            service_ext: Default::default(),
            label: None,
        };
        B2buaSut::builder(backend(route, Ok(CallTreatment::Reject(reject))))
            .limiter(refuse_on("cap", fail_open()))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let resp = call.expect(503).await;
    assert_eq!(resp.raw(HeaderName::Reason).collect::<Vec<_>>(), [ITS_OWN]);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}

/// A callee's own 503 is relayed as it is under the route's statements: it is no
/// error of this stack's.
#[tokio::test(start_paused = true)]
async fn a_relayed_5xx_takes_the_routes_reason_as_any_final() {
    let s = B2buaScene::with_b2bua("bare-503-relayed", |bob_port| {
        let route = stating_route(bob_port, None, false);
        B2buaSut::builder(backend(route, Err("unused".into())))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(503, "Service Unavailable").await;
    s.bob.receive("ACK").await;
    let resp = call.expect(503).await;
    assert_eq!(resp.raw(HeaderName::Reason).collect::<Vec<_>>(), [STATED_REASON]);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _ = s.finish().await;
}
