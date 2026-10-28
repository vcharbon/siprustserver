//! RFC 3261 §18.2.2 with RFC 3581 §4: a caller behind a port-translating NAT
//! (her Via names an address other than her socket, with `;rport`) gets every
//! response of a request at the socket that request came from, the 100 and
//! the final alike, each echoing `received` and `rport` (§18.2.1). The final
//! comes from every path that can answer an initial INVITE: a decision reject,
//! a decision error or timeout (ADR-0022), a store fault, a relayed callee
//! reject; and inside the dialog, the answers to a BYE and to a re-INVITE.
//! Alice only listens on her socket, so a response she receives reached it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::reject;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::store::{StoreFaultPoint, StoreFaults};
use b2bua_harness::{B2buaScene, B2buaSut, B2buaSutBuilder};
use scenario_harness::Agent;
use sip_message::generators::InDialogMethod;
use sip_message::header::Rport;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The scene with alice behind a NAT: her Via names a fresh address no socket
/// holds.
async fn behind_nat(name: &str, build: impl FnOnce(u16) -> B2buaSutBuilder) -> (B2buaScene, Agent) {
    let s = B2buaScene::with_b2bua(name, build).await;
    let alice = s.alice.clone().behind_nat(s.h.next_addr());
    (s, alice)
}

/// `resp` names alice's socket on its top Via: `received` her IP, `rport`
/// her port.
fn echoes_her_socket(resp: &SipResponse, alice: &Agent) {
    let via = resp.top_via();
    let status = resp.status();
    let ip = alice.addr().ip().to_string();
    assert_eq!(via.received(), Some(ip.as_str()), "{status} echoes received (§18.2.1): {via:?}");
    assert_eq!(
        via.rport(),
        Some(Rport::Observed(alice.addr().port())),
        "{status} echoes rport=<source port> (RFC 3581 §4): {via:?}"
    );
}

/// A decision backend whose `/call/new` fails.
struct FailingDecisionEngine;

#[async_trait]
impl CallDecisionEngine for FailingDecisionEngine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("backend down".into()))
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("backend down".into()))
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("backend down".into()))
    }
}

/// A decision backend whose `/call/new` never answers.
struct HangingDecisionEngine;

#[async_trait]
impl CallDecisionEngine for HangingDecisionEngine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        std::future::pending().await
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        std::future::pending().await
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn a_decision_reject_reaches_the_socket_the_100_reached() {
    let decision =
        Arc::new(ScriptedDecisionEngine::builder().fallback(|_| reject(403, "Forbidden")).build());
    let (s, alice) = behind_nat("nat-decision-reject", |_| B2buaSut::builder(decision)).await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    echoes_her_socket(&call.expect(100).await, &alice);
    echoes_her_socket(&call.expect(403).await, &alice);

    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_decision_error_503_reaches_the_socket_the_100_reached() {
    let (s, alice) =
        behind_nat("nat-decision-error", |_| B2buaSut::builder(Arc::new(FailingDecisionEngine)))
            .await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    echoes_her_socket(&call.expect(100).await, &alice);
    echoes_her_socket(&call.expect(503).await, &alice);

    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_decision_timeout_503_reaches_the_socket_the_100_reached() {
    let (s, alice) =
        behind_nat("nat-decision-timeout", |_| B2buaSut::builder(Arc::new(HangingDecisionEngine)))
            .await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    echoes_her_socket(&call.expect(100).await, &alice);
    // The default decision deadline is 5 s (ADR-0022).
    s.h.advance(Duration::from_millis(5_500)).await;
    echoes_her_socket(&call.expect(503).await, &alice);

    let _report = s.finish().await;
}

/// The store-fault refusal of an initial INVITE leaves in the turn that reads
/// it, before the transaction's 100 is due: it is the INVITE's first response.
#[tokio::test(start_paused = true)]
async fn a_store_fault_refusal_reaches_the_callers_socket() {
    let faults = StoreFaults::default();
    faults.arm(StoreFaultPoint::LiveInitialInvite);
    let armed = faults.clone();
    let (s, alice) = behind_nat("nat-store-fault", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).with_store_faults(armed)
    })
    .await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(500)).await;
    echoes_her_socket(&call.expect(500).await, &alice);
    faults.disarm(StoreFaultPoint::LiveInitialInvite);

    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_relayed_callee_reject_reaches_the_socket_the_100_reached() {
    let (s, alice) =
        behind_nat("nat-callee-reject", |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port))
            .await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    echoes_her_socket(&call.expect(100).await, &alice);
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    echoes_her_socket(&call.expect(486).await, &alice);

    let _report = s.finish().await;
}

/// Her BYE and her re-INVITE, inside the dialog: the 200 to the BYE and the
/// relayed 488 to the re-INVITE reach her socket.
#[tokio::test(start_paused = true)]
async fn in_dialog_answers_reach_the_callers_socket() {
    let (s, alice) =
        behind_nat("nat-in-dialog", |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;

    let mut call = alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    echoes_her_socket(&call.expect(180).await, &alice);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    echoes_her_socket(&call.expect(200).await, &alice);
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    s.bob.receive("INVITE").await.respond(488, "Not Acceptable Here").await;
    s.bob.receive("ACK").await;
    echoes_her_socket(&reinvite.expect(488).await, &alice);

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    echoes_her_socket(&bye.expect(200).await, &alice);

    let _report = s.finish().await;
}
