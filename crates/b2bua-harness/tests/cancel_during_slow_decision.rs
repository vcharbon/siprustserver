//! Regression: a CANCEL that races a *slow* new-call decision must still tear
//! the call down cleanly.
//!
//! WHAT THIS GUARDS. The initial INVITE is handled out of the synchronous rule
//! chain by [`b2bua::initial_invite::handle_initial_invite`], which `await`s the
//! decision backend (in production an HTTP adapter — `/call/new`). That `await`
//! is held INSIDE the per-call FIFO worker, under the per-call state lock, so a
//! slow backend parks the whole call's handler. The question this test answers:
//! if the caller hangs up (CANCEL) while that decision is still in flight, is
//! the CANCEL still honoured and the call fully reaped?
//!
//! The answer is yes, by two independent mechanisms working together:
//!
//!  1. The CANCEL's `200 OK` + the INVITE's `487 Request Terminated` are emitted
//!     by the *transaction layer* the instant the CANCEL matches the in-progress
//!     INVITE server txn (sip-txn `handle_cancel`) — it does NOT wait on the
//!     router / decision. So the caller is released immediately.
//!
//!  2. The `Cancelled` event is dispatched to the SAME per-call FIFO as the
//!     in-flight INVITE body and so queues strictly BEHIND it (`PerCallDispatcher`
//!     runs one body at a time). When the slow decision finally returns and the
//!     INVITE body finishes building the b-leg, the queued `handle-cancel` rule
//!     runs next, CANCELs the freshly-created b-leg, and drives the call to
//!     Terminated. FIFO ordering is what makes this deterministic — the cancel
//!     can never be processed against half-built state, nor be lost because the
//!     call did not exist yet (the INVITE body `create`s the call synchronously,
//!     before its first `await`).
//!
//! Without (2) a CANCEL arriving mid-decision could be dropped as "unroutable"
//! (no call yet) or applied before the b-leg exists, leaking the b-leg the slow
//! decision is about to create — the failure mode this test pins shut.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// How long the decision backend (HTTP adapter) takes to answer `/call/new`.
/// Made large relative to one network transit (~100 ms) so the CANCEL is
/// guaranteed to arrive — and be fully processed — while the decision is still
/// parked. Under the paused runtime this is virtual time, so the test is fast.
const DECISION_DELAY: Duration = Duration::from_secs(5);

/// A [`CallDecisionEngine`] that sleeps before delegating to `inner` — the test
/// stand-in for a slow HTTP `/call/new` round-trip. Only `new_call` is delayed
/// (the path under test); failure/refer pass straight through.
struct SlowDecisionEngine {
    inner: Arc<dyn CallDecisionEngine>,
    delay: Duration,
}

#[async_trait]
impl CallDecisionEngine for SlowDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(self.delay).await;
        self.inner.new_call(req).await
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

#[tokio::test(start_paused = true)]
async fn cancel_during_slow_decision_tears_down_cleanly() {
    let h = Harness::new("b2bua-cancel-slow-decision");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let decision = Arc::new(SlowDecisionEngine {
        inner: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)),
        delay: DECISION_DELAY,
    });
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5081", decision).await;

    // ── alice INVITEs through the B2BUA; the decision backend stalls ───────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    // Let the INVITE be delivered and the call materialise — the handler body
    // `create`s the call, then parks on the slow decision (well under
    // DECISION_DELAY, so it is still in flight after this advance).
    h.advance(Duration::from_millis(500)).await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "the call exists (a-leg built) while the decision is in flight",
    );
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no b-leg INVITE yet — the decision has not returned, so no route exists",
    );

    // ── alice hangs up mid-decision: CANCEL ───────────────────────────────────
    // The txn layer answers immediately, independent of the parked decision:
    // 200 OK to the CANCEL, 487 Request Terminated to the INVITE.
    let mut cxl = call.cancel().await;
    cxl.expect(200).await; // 200 OK to the CANCEL itself
    call.expect(487).await; // 487 Request Terminated on the INVITE

    // ── The slow decision finally returns ─────────────────────────────────────
    // Advance past the decision delay so the parked INVITE body resumes: it
    // builds the b-leg and sends it to bob, then the queued `handle-cancel` runs
    // and CANCELs that b-leg. (We advance explicitly rather than letting
    // `bob.receive` auto-advance, because the paused runtime would otherwise trip
    // that call's internal 2 s recv-timeout before the 5 s decision returns.)
    // bob therefore sees an INVITE immediately followed by a CANCEL.
    h.advance(DECISION_DELAY + Duration::from_secs(1)).await;
    let mut b_inv = bob.receive("INVITE").await;
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await; // 200 to the CANCEL
    b_inv.respond(487, "Request Terminated").await; // 487 to the b-leg INVITE

    // ── The call must be fully reaped (active_calls -> 0) ──────────────────────
    // The B2BUA's b-leg client txn auto-ACKs the 487 (non-2xx) and the call
    // resolves to Terminated — well before the 32 s TerminatingTimeout backstop,
    // so a 1 s settle suffices. (A late ACK to bob may sit undelivered in its
    // queue — finish() only gates the RFC CSeq rules, not structural in-flight,
    // so that is fine here.)
    h.advance(Duration::from_secs(1)).await;
    for _ in 0..50 {
        if b2bua.metrics().removals_total() == b2bua.metrics().creations_total() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let m = b2bua.metrics();
    assert_eq!(
        m.removals_total(),
        m.creations_total(),
        "a CANCEL racing a slow decision must still reap the call (active_calls -> 0); \
         got creations={} removals={}",
        m.creations_total(),
        m.removals_total(),
    );
    assert_eq!(b2bua.active_calls(), 0, "no live call left");
    assert_eq!(b2bua.lock_count(), 0, "no stranded per-call lock");

    let _report = h.finish().await;
}
