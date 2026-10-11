//! A failure consult the decision backend leaves unanswered: the caller gets
//! the stack's own bare `503`, never the failed leg's final, and the call's
//! decision log gains no mark.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{fail_open, refuse_on};
use call::DecisionKind;
use call::MessageDirection::Authored;
use scenario_harness::Harness;
use sip_message::header::HeaderName;

use crate::decision_log::{events, marks, stamped, Sut, ALICE, BOB, CAROL, OFFER};

/// A scripted engine whose failure consult never answers: the fold is the
/// stack's own resolution.
struct ErringFailover(ScriptedDecisionEngine);

#[async_trait]
impl CallDecisionEngine for ErringFailover {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.0.new_call(req).await
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("down".into()))
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.0.call_refer(req).await
    }
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.0.call_release(req).await
    }
}

/// An unanswered failure consult is no decision: the caller gets the
/// stack's own bare `503`, no `Reason` and no `Retry-After`, never the failed
/// leg's final nor its headers; it adds no mark, and everything it emits keeps the route's
/// ordinal.
#[tokio::test(start_paused = true)]
async fn an_unanswered_failure_consult_answers_503_and_marks_nothing() {
    let h = Harness::new("decision-log-consult-error");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let decision = Arc::new(ErringFailover(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.label = Some("first".into());
                NewCallResponse::Route(r)
            })
            .build(),
    ));
    let sut = Sut::spawn(&h, decision).await;

    let mut call = alice.invite(&carol).with_sdp(OFFER).through(sut.addr).send().await;
    // The failed leg's final states what a relayed final would carry.
    carol
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Reason", "Q.850;cause=17")
        .with_header("Retry-After", "60")
        .await;
    carol.receive("ACK").await;
    let resp = call.expect(503).await;
    assert_bare_503(&resp);

    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::Route, Some("a"), Some("first"))]);
    let a = stamped(&done.a_leg.messages.entries);
    assert_eq!(a[2], (Authored, "INVITE", Some(503), 1), "the stack's own final: {a:?}");
    assert!(a[2..].iter().all(|r| r.3 == 1), "{a:?}");
    assert!(events(&done)[1..].iter().all(|e| e.2 == 1), "{:?}", events(&done));

    let _report = h.finish().await;
}

/// The failure consult a limiter refusal raises goes unanswered: the caller
/// gets the stack's own bare `503`, no leg is dialed, and nothing is marked.
#[tokio::test(start_paused = true)]
async fn an_unanswered_limiter_failure_consult_answers_503() {
    let h = Harness::new("decision-log-limiter-consult-error");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(ErringFailover(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.call_limiter = vec![LimiterEntry { id: "cap".into(), limit: 1 }];
                r.label = Some("capped".into());
                NewCallResponse::Route(r)
            })
            .build(),
    ));
    let sut = Sut::spawn_with(&h, decision, Some(refuse_on("cap", fail_open())), Vec::new()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let resp = call.expect(503).await;
    assert_bare_503(&resp);

    let done = sut.assert_reaped().await;
    assert!(marks(&done).is_empty(), "no decision: {:?}", marks(&done));
    assert!(done.b_legs.is_empty(), "the refused route dialed nothing");

    let _report = h.finish().await;
}

/// The stack's own `503` for a decision backend that gave no answer: the RFC
/// phrase, no `Reason` and no `Retry-After`.
fn assert_bare_503(resp: &sip_message::SipResponse) {
    assert_eq!(resp.reason(), "Service Unavailable");
    assert!(
        !resp.has(&HeaderName::Reason),
        "no Reason: {:?}",
        resp.raw(HeaderName::Reason).collect::<Vec<_>>()
    );
    assert!(
        !resp.has(&HeaderName::RetryAfter),
        "no Retry-After: {:?}",
        resp.raw(HeaderName::RetryAfter).collect::<Vec<_>>()
    );
}
