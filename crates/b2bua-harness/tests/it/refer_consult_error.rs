//! A `/call/refer` consult the decision backend leaves unanswered (its
//! error): the referrer's terminating NOTIFY carries `SIP/2.0 503`, as if the
//! transfer target had answered 503 (ADR-0043), and the call stays up. A
//! consult the engine refuses with a code of its own notifies that code.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaSut, ANSWER_SDP, OFFER_SDP};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::SubscriptionState;

/// Routes with the REFER feature on; every `/call/refer` consult fails with
/// the error stated.
struct ErringRefer(ScriptedDecisionEngine, fn() -> CallDecisionError);

#[async_trait]
impl CallDecisionEngine for ErringRefer {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.0.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.0.call_failure(req).await
    }
    async fn call_refer(
        &self,
        _: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err((self.1)())
    }
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.0.call_release(req).await
    }
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_refer_consult_notifies_503() {
    let body = refer_consult_sipfrag("refer-consult-error", || {
        CallDecisionError::Unavailable("backend down".into())
    })
    .await;
    assert!(body.starts_with("SIP/2.0 503 Service Unavailable"), "{body:?}");
}

#[tokio::test(start_paused = true)]
async fn a_refer_consult_refused_with_a_code_notifies_that_code() {
    let body = refer_consult_sipfrag("refer-consult-refused", || CallDecisionError::Refused {
        code: 403,
        reason: Some("Forbidden".into()),
        update_headers: None,
    })
    .await;
    assert!(body.starts_with("SIP/2.0 403 Forbidden"), "{body:?}");
}

/// One call whose REFER consult fails with `error`: the sipfrag of the
/// referrer's terminating NOTIFY, the call then ended by the caller.
async fn refer_consult_sipfrag(name: &str, error: fn() -> CallDecisionError) -> String {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let engine =
        ErringRefer(ScriptedDecisionEngine::route_all_with_refer("127.0.0.1", 5070), error);
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", "<sip:charlie@example.com>")
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut terminated = bob.receive("NOTIFY").await;
    let state = terminated
        .request()
        .header::<SubscriptionState>()
        .expect("a Subscription-State")
        .expect("readable");
    assert!(state.is("terminated"));
    let body = String::from_utf8_lossy(terminated.request().body()).to_string();
    terminated.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    body
}
