//! A non-INVITE request the B2BUA took off the transaction layer but discarded
//! before any handler ran — the call's queue full, or the call cap reached —
//! leaves no transaction behind to absorb its retransmissions. The peer's
//! Timer E copy (RFC 3261 §17.1.2.2) is admitted afresh and draws its answer
//! once the B2BUA has room, instead of silence until the peer's Timer F.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua_harness::{establish, settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::{SipMessage, SipResponse};

/// Routes the first call to bob; every later `new_call` never resolves, so
/// its INVITE body holds a handler permit until the decision deadline.
struct RouteFirstThenHang {
    inner: ScriptedDecisionEngine,
    calls: AtomicUsize,
}

#[async_trait]
impl CallDecisionEngine for RouteFirstThenHang {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.inner.new_call(req).await
        } else {
            std::future::pending().await
        }
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// What the caller side of a confirmed dialog needs to write a BYE by hand,
/// so the test can retransmit the very same datagram (RFC 3261 §17.1.2.2).
struct DialogIds {
    call_id: String,
    from_uri: String,
    from_tag: String,
    to_uri: String,
    to_tag: String,
    /// The B2BUA's Contact: the remote target, carrying its `callRef`.
    remote_target: String,
}

impl DialogIds {
    fn of(answer: &SipResponse) -> Self {
        DialogIds {
            call_id: answer.call_id().as_str().to_string(),
            from_uri: answer.from().uri().to_string(),
            from_tag: answer.from().tag().expect("the INVITE carried a From-tag").to_string(),
            to_uri: answer.to().uri().to_string(),
            to_tag: answer.to().tag().expect("the 2xx carries a To-tag").to_string(),
            remote_target: answer
                .contacts()
                .as_slice()
                .first()
                .map(|c| c.uri().to_string())
                .expect("the 2xx carries a Contact"),
        }
    }

    /// The BYE `from` sends in this dialog, as one datagram.
    fn bye(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "BYE {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {via};branch=z9hG4bK-{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <{from_uri}>;tag={from_tag}\r\n\
             To: <{to_uri}>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} BYE\r\n\
             Content-Length: 0\r\n\r\n",
            ruri = self.remote_target,
            via = from.addr(),
            from_uri = self.from_uri,
            from_tag = self.from_tag,
            to_uri = self.to_uri,
            to_tag = self.to_tag,
            call_id = self.call_id,
        )
        .into_bytes()
    }
}

/// Set up a call by hand and keep the caller's 2xx.
async fn establish_keeping_answer(
    caller: &Agent,
    callee: &Agent,
    b2bua: SocketAddr,
) -> (scenario_harness::Dialog, SipResponse) {
    let mut call = caller.invite(callee).with_sdp(OFFER_SDP).through(b2bua).send().await;
    let mut uas = callee.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    callee.receive("ACK").await;
    (dialog, answer)
}

/// The final responses to a BYE queued at `agent` after `wait`.
async fn bye_finals(h: &Harness, agent: &Agent, wait: Duration) -> Vec<u16> {
    h.advance(wait).await;
    let mut out = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == "BYE" && r.status() >= 200 {
                out.push(r.status());
            }
        }
    }
    out
}

/// One handler permit, held by a second call parked on its decision; a
/// per-call queue one deep. alice's two INFOs fill the call's worker and
/// queue, so her BYE is dropped at dispatch. Its retransmission after the
/// permit frees is processed: the BYE is answered and relayed, the call ends.
#[tokio::test(start_paused = true)]
async fn a_bye_the_full_per_call_queue_dropped_is_answered_on_its_retransmission() {
    let s = B2buaScene::with_b2bua("b2bua-bye-dropped-queue-full", |bob_port| {
        B2buaSut::builder(Arc::new(RouteFirstThenHang {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            calls: AtomicUsize::new(0),
        }))
        .tune(|c| {
            c.event_dispatch_concurrency = 1;
            c.per_call_queue_depth = 1;
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);

    // carol's call parks on its decision and holds the one handler permit.
    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;

    // The first INFO waits on the permit in the call's worker, the second
    // fills the call's queue, the BYE is dropped.
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let bye = ids.bye(&s.alice, dialog.local_cseq() + 1, "bye-queue-full");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 1, "the BYE is dropped at dispatch");

    // Timer E copies while the call is still blocked: dropped again, silent.
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(1_000)).await.is_empty());
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 2, "the copy is admitted and dropped");
    assert_eq!(s.b2bua.txn_metrics().unanswered_forgotten(), 2, "one forget per dropped copy");

    // carol's decision deadline frees the permit; the INFOs are relayed.
    parked.expect(503).await;
    for info in [&mut info1, &mut info2] {
        let mut uas = s.bob.receive("INFO").await;
        uas.respond(200, "OK").await;
        info.expect(200).await;
    }

    // The next Timer E copy reaches the call and is answered.
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert_eq!(
        bye_finals(&s.h, &s.alice, Duration::from_millis(500)).await,
        vec![200],
        "the retransmitted BYE is answered, not absorbed"
    );
    let mut relayed = s.bob.receive("BYE").await;
    relayed.respond(200, "OK").await;

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// The call cap is one. A late BYE for a call that has ended finds no queue
/// while another call holds the cap, and is dropped at dispatch. Once that
/// call ends, the BYE's retransmission reaches the orphan path and draws 481
/// (RFC 3261 §15.1.2).
#[tokio::test(start_paused = true)]
async fn a_late_bye_dropped_at_the_call_cap_draws_481_once_the_cap_frees() {
    let s = B2buaScene::with_b2bua("b2bua-bye-dropped-at-cap", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.per_call_queue_cap = 1)
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut ended, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);
    let bye_cseq = ended.local_cseq() + 2;
    s.hangup(&mut ended).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "a BYE in a dialog the B2BUA no longer holds is the deviation under test",
    );

    let mut holding = establish(&carol, &s.bob, s.b2bua.addr).await;
    settle_until(|| s.b2bua.active_calls() == 1).await;

    let bye = ids.bye(&s.alice, bye_cseq, "late-bye-at-cap");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(300)).await.is_empty());
    assert_eq!(s.b2bua.metrics().cap_drops_total(), 1, "the BYE is dropped at the cap");
    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert!(bye_finals(&s.h, &s.alice, Duration::from_millis(1_000)).await.is_empty());
    assert_eq!(s.b2bua.metrics().cap_drops_total(), 2, "the copy is admitted and dropped");
    assert_eq!(s.b2bua.txn_metrics().unanswered_forgotten(), 2, "one forget per dropped copy");

    // Freeing the cap: carol hangs up.
    scenario_harness::callflow::hangup(&mut holding, &s.bob).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;

    s.alice.try_send_datagram(&bye, s.b2bua.addr).await.expect("the BYE leaves");
    assert_eq!(
        bye_finals(&s.h, &s.alice, Duration::from_millis(500)).await,
        vec![481],
        "the retransmitted BYE reaches the orphan path, not a transaction absorbing it"
    );
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
