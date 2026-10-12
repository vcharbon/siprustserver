//! **When a new call first reaches its backup.** The caller's INVITE draws the
//! worker's 100 Trying the instant its server transaction is born (RFC 3261
//! §17.2.1), and the stateless front proxy relays that 100 (§16.11), so from
//! then on the caller sends no retransmission that could re-place the call.
//! The call is written to its backup at its birth, before the routing decision
//! is asked for, so a node death after the 100 leaves a replica behind for the
//! departure sweep to answer (ADR-0047). This cell measures the window from the
//! caller's 100 to the first replica with a decision that takes a known time
//! to answer: one replication stream hop, the decision's time no part of it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// How long the routing decision takes to answer.
const DECISION_DELAY: Duration = Duration::from_millis(1_000);
/// How long after the call's birth the replica may take to land.
const STREAM_HOP_BOUND: Duration = Duration::from_millis(50);
/// The probe's resolution.
const PROBE_STEP: Duration = Duration::from_millis(5);

/// A decision engine that answers `new_call` after [`DECISION_DELAY`].
struct SlowDecision(ScriptedDecisionEngine);

#[async_trait]
impl CallDecisionEngine for SlowDecision {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(DECISION_DELAY).await;
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
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.0.call_refer(req).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_new_call_reaches_its_backup_before_its_routing_decision_answers() {
    let mut fh = FailoverHarness::new("new-call-replication-window", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let decision = || -> Arc<dyn CallDecisionEngine> {
        Arc::new(SlowDecision(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)))
    };
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let noop = || Arc::new(NoopLimiter);
    let w_b1 = fh.spawn_worker_limited("b1", "b1", B1, &["b2"], rt, ob, decision(), noop()).await;
    let w_b2 = fh.spawn_worker_limited("b2", "b2", B2, &["b1"], rt, ob, decision(), noop()).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── The caller hears the worker's 100; the replica follows at once ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let heard_at = fh.now_ms();
    let replicated = async || {
        w_b1.scan_one_backed_up("b2").await.is_some()
            || w_b2.scan_one_backed_up("b1").await.is_some()
    };
    let routed = || fh.sip_entries().iter().any(|e| e.to == bob.addr());
    while !replicated().await {
        assert!(fh.now_ms() - heard_at < 2 * DECISION_DELAY.as_millis() as i64, "never replicated");
        fh.advance(PROBE_STEP).await;
    }
    let window = Duration::from_millis((fh.now_ms() - heard_at) as u64);
    assert!(!routed(), "the replica precedes the routing decision");
    assert!(
        window <= STREAM_HOP_BOUND,
        "from the caller's 100 to the first replica: one stream hop, measured {window:?}",
    );
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref =
        backup.scan_one_backed_up(&pri_ord).await.expect("the primary's backup holds it");

    // ── The call completes and ends ─────────────────────────────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop((w_b1, w_b2, proxy));
}
