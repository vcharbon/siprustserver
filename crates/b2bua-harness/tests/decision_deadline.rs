//! ADR-0022 — the initial-INVITE final-response guarantee at the decision seam.
//!
//! sip-txn auto-answers **100 Trying** the instant the INVITE server txn is
//! born, BEFORE the router or the decision backend ever see the call. From that
//! moment the caller has stopped retransmitting and is waiting on us. These
//! tests pin the two error legs of the guarantee the CLAUDE/ADR contract
//! states — *"either the call is accepted and forwarded, or it is rejected
//! with 503 within the decision deadline (default 5 s)"* — against the two
//! ways a decision backend goes wrong:
//!
//!  1. **Hang / slow HTTP adapter** (incl. any third-party `CallDecisionEngine`
//!     that forgot its own timeout): the core-level [`DeadlineDecisionEngine`]
//!     wrap converts the parked await into the ordinary decision-error 503 at
//!     `call_control_timeout_ms` (default 5000 ms — the TS
//!     `CALL_CONTROL_NEW_CALL_TIMEOUT_MS` parity).
//!  2. **Internal error (panic) inside the INVITE body**: the dispatcher
//!     isolates the panic, the reaper's strike-1 `fatal-error` verdict reaps
//!     through the rules, and the `→ terminated` funnel's unanswered-a-leg
//!     invariant (`invariants::enforce`) synthesizes the 503 the crashed body
//!     never sent. No 5 s wait — the hook fires immediately.
//!
//! The genuinely-wedged variant (deadline DISABLED, reaper abort-escalation
//! ladder, late 503) lives in `reaper.rs::wedged_setup_is_aborted_and_reaped`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{establish, settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER_SDP: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The dead-HTTP-backend shape: `new_call` never resolves. Unlike the reaper
/// wedge test, the deadline stays at its DEFAULT — the point is that the core
/// bounds this without any tuning.
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

/// The buggy-adapter shape: `new_call` panics (a third-party adapter throwing
/// on a malformed response body, an `unwrap` on a poisoned lock, …).
struct PanickingDecisionEngine;

#[async_trait]
impl CallDecisionEngine for PanickingDecisionEngine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        panic!("probe: decision adapter blew up in /call/new");
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        panic!("probe: decision adapter blew up in /call/failure");
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        panic!("probe: decision adapter blew up in /call/refer");
    }
}

/// A hung decision backend must NOT strand the caller behind the auto-100:
/// the default 5 s deadline converts it into the ordinary decision-error 503.
/// The advance points bracket the deadline to pin the bound, not just the
/// eventual delivery.
#[tokio::test(start_paused = true)]
async fn hung_decision_is_rejected_503_at_the_default_deadline() {
    let h = Harness::new("decision-deadline-hang");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    // DEFAULT config — no tune: the guarantee must hold out of the box.
    let b2bua = B2buaSut::builder(Arc::new(HangingDecisionEngine))
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;

    // Just short of the deadline: the call exists (a-leg built, decision
    // parked), nothing decided — no b-leg INVITE, no CDR, no final.
    h.advance(Duration::from_millis(4_500)).await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "the call is parked on the hung decision, still live",
    );
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no route decided — no b-leg INVITE",
    );
    assert!(b2bua.cdr_records().is_empty(), "no final produced before the deadline");

    // Cross the 5 s mark: the deadline fires, the decision-error path answers.
    h.advance(Duration::from_millis(1_000)).await;
    call.expect(503).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR for the deadline-rejected call");
    assert!(
        cdrs[0].events.iter().any(|e| e.status_code == Some(503)),
        "the CDR carries the 503 reject",
    );

    let _report = h.finish().await;
}

/// The per-call global cap is the one full-queue path that fires BEFORE a call
/// exists (`dispatch` drops a brand-new call_ref's body when the live-queue map
/// is full). ADR-0022 sheds a new initial INVITE there with a stateless 503
/// instead of silently, so a caller who already heard the auto-100 still gets a
/// final. cap = 1: one live call fills the map, the next NEW INVITE is shed.
#[tokio::test(start_paused = true)]
async fn initial_invite_at_the_per_call_cap_is_shed_503_not_dropped() {
    let h = Harness::new("decision-deadline-cap");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let carol = h.agent("carol", "127.0.0.1:5062").await;
    let b2bua = B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)))
        .tune(|c| c.per_call_queue_cap = 1) // exactly one live per-call queue allowed
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    // Call 1 establishes and stays up → its per-call queue occupies the cap.
    let _d1 = establish(&alice, &bob, b2bua.addr).await;
    settle_until(|| b2bua.active_calls() == 1).await;

    // Call 2 is a brand-new INVITE while the map is at cap → stateless 503,
    // never a silent drop. The caller heard the auto-100, then gets its final.
    let mut call2 = carol.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(500)).await;
    call2.expect(503).await;

    // Call 1 is untouched (still the single live call); no CDR/limiter/state was
    // born for the shed call 2 (stateless — like the Tier-3 gate).
    assert_eq!(b2bua.active_calls(), 1, "the shed INVITE created no call");

    let _report = h.finish().await;
}

/// A decision adapter that PANICS must not strand the caller either: strike-1
/// `fatal-error` reaps through the rules and the `→ terminated` funnel
/// synthesizes the 503 the dead body never sent — immediately, no deadline
/// involved.
#[tokio::test(start_paused = true)]
async fn panicking_decision_is_rejected_503_immediately() {
    let h = Harness::new("decision-deadline-panic");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(Arc::new(PanickingDecisionEngine))
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;
    let _ = bob; // never reached: the panic fires before any route exists

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(b2bua.addr).send().await;

    // The panic hook → fatal-error verdict → reap → synthesized 503 all ride
    // immediate dispatch; one short advance covers transit.
    h.advance(Duration::from_secs(1)).await;
    call.expect(503).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.metrics().handler_panics_total(), 1, "the panic was observed, not swallowed");
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR despite the panic");
    let reasons: Vec<String> = cdrs[0].events.iter().filter_map(|e| e.reason.clone()).collect();
    assert!(
        reasons.iter().any(|r| r == "handler-panic"),
        "the CDR names the strike-1 panic reap: {reasons:?}",
    );
    assert!(
        reasons.iter().any(|r| r == "unanswered_at_termination"),
        "the CDR records the synthesized final: {reasons:?}",
    );

    let _report = h.finish().await;
}
