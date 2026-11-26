//! **The fallback 100 of a call whose outbox does not drain.** A new call's
//! 100 Trying follows its creation write through the call's outbox. The turn
//! that sends the routing decision request also arms the transaction layer's
//! §17.2.1 bound (ADR-0070 §2): when the outbox holds the 100 past that bound,
//! the layer sends it itself, 200 ms after the arming, and the rider's later
//! release sends nothing more. The call then completes as any other.
//!
//! Beyond the failover sweep: the fallback 100 of a backed call.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use failover_harness::{assert_call_fully_released, total_cdrs_for, FailoverHarness};
use scenario_harness::callflow::{hangup, ANSWER_SDP, OFFER_SDP};

use crate::departure_support::call_id_placed_on;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The call's primary, the worker its Call-ID is placed on.
const PRIMARY: &str = "b1";
/// The §17.2.1 bound on the INVITE's first provisional.
const TRYING_BOUND: Duration = Duration::from_millis(200);
/// How long the primary holds the rider of the caller's 100 once its outbox
/// released it: well past the bound.
const RIDER_HOLD: Duration = Duration::from_millis(600);
/// How long the routing decision takes to answer: past the rider's hold, so
/// no response of the call's own overtakes the 100.
const DECISION_DELAY: Duration = Duration::from_millis(1_000);
/// How far short of the bound the caller is read for an early 100.
const MARGIN: Duration = Duration::from_millis(20);
const _: () = assert!(TRYING_BOUND.as_millis() < RIDER_HOLD.as_millis());
const _: () = assert!(RIDER_HOLD.as_millis() < DECISION_DELAY.as_millis());

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

/// The primary's outbox holds the caller's 100: the caller hears the
/// transaction layer's 100 at the bound counted from the consult, before the
/// rider goes, and once, before the callee is dialled.
#[tokio::test(start_paused = true)]
async fn the_layer_sends_the_100_an_outbox_holds_past_the_bound() {
    let name = "fallback-100-behind-a-stalled-outbox";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
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
    fh.turn_points().pace_riders(PRIMARY, RIDER_HOLD);

    let sent_at = fh.now_ms();
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER_SDP)
        .with_call_id(call_id_placed_on(PRIMARY, "fallback-100"))
        .through(proxy.addr())
        .send()
        .await;
    sip_clock::testkit::advance_settled(TRYING_BOUND - MARGIN).await;
    let early: Vec<_> = fh.sip_entries().into_iter().filter(|e| e.to == alice.addr()).collect();
    assert!(early.is_empty(), "no 100 inside the bound: the rider holds it: {early:?}");
    call.expect(100).await;
    let heard = Duration::from_millis((fh.now_ms() - sent_at) as u64);
    assert!(heard >= TRYING_BOUND, "the layer's 100, past the bound: {heard:?}");
    assert!(heard < RIDER_HOLD, "the 100 leaves before the rider: {heard:?}");
    assert!(!fh.sip_entries().iter().any(|e| e.to == bob.addr()), "the 100 precedes the dial");
    let backup = &w_b2;
    let call_ref =
        backup.scan_one_backed_up(PRIMARY).await.expect("the primary's backup holds the call");

    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hangup(&mut dialog, &bob).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let trying_to_alice = fh
        .sip_entries()
        .iter()
        .filter(|e| e.to == alice.addr() && e.raw.starts_with(b"SIP/2.0 100 "))
        .count();
    assert_eq!(trying_to_alice, 1, "one 100: the rider's release sends none");
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}
