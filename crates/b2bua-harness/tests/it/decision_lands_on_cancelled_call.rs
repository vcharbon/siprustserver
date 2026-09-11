//! A `/calls` decision result landing on a call the CALLER already CANCELed
//!.
//!
//! The caller gives up while the routing decision is still in flight: the txn
//! layer finalizes the initial-INVITE transaction at once (200 to the CANCEL,
//! 487 to the INVITE, ACKed), and the decision lands afterwards. Whatever it
//! decides is moot and must be DROPPED whole:
//!
//!  - a ROUTE must not launch a b-leg — the callee would be rung (and start
//!    billing) for a call that no longer has a caller;
//!  - a REJECT/REDIRECT must not author a second final on the a-leg's
//!    completed transaction (RFC 3261 §17.2.1 — the 487 is the one final).
//!
//! Two seams carry the guarantee: the initial-INVITE decision application
//! (`new_call` parks the per-call FIFO, so the queued `Cancelled` event cannot
//! reach the model — the router reads the setup-CANCEL mark instead), and the
//! async decision folds (`call-failure-result` / `call-release-result`), which
//! land through the rule chain and read the call's own
//! `Terminating`/`Terminated` state.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::{reject, route_to};
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallTreatment, NewCallRequest,
    NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{establish, hangup, invite_final_statuses, settle_until, B2buaSut};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::{Harness, RunReport};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The scripted BL round trip the caller's CANCEL races (BC_02: up to the
/// adapter's 1 s budget — inside the 5 s decision deadline).
const DECISION_DELAY: Duration = Duration::from_millis(900);

/// Delay `new_call` and/or `call_failure` before delegating — the in-flight
/// decision window the caller's CANCEL lands in.
struct DelayedDecisionEngine {
    new_call_delay: Duration,
    failure_delay: Duration,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for DelayedDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(self.new_call_delay).await;
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        tokio::time::sleep(self.failure_delay).await;
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// The distinct Call-IDs of INVITE requests delivered to `to` — retransmits of
/// the same b-leg INVITE dedup to one entry; a second entry is a fresh leg
/// launched toward a callee whose caller is gone.
fn distinct_invite_call_ids(report: &RunReport, to: SocketAddr) -> usize {
    let mut ids: Vec<String> = report
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) if r.method() == Method::Invite => {
                Some(r.call_id().as_str().to_string())
            }
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

/// ROUTE variant: the delayed route lands on the CANCELed call and is dropped
/// whole — bob is never dialed, alice hears nothing after her 487, and the
/// queued `handle-cancel` terminates the call at once.
#[tokio::test(start_paused = true)]
async fn route_decision_landing_after_the_callers_cancel_is_dropped() {
    let h = Harness::new("route-lands-on-cancelled-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(DelayedDecisionEngine {
        new_call_delay: DECISION_DELAY,
        failure_delay: Duration::ZERO,
        inner: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
    });
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    // The INVITE is at the SUT (auto-100 sent), the route still in flight.
    h.advance(Duration::from_millis(200)).await;

    // ── the caller gives up while the route is undecided ─────────────────────
    // The a-leg server txn completes at once: 200 to the CANCEL, 487 to the
    // INVITE (ACKed by the txn-layer auto-ACK) — the ONLY final it ever takes.
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no route decided yet — nothing at bob",
    );

    // ── cross the decision's landing ─────────────────────────────────────────
    h.advance(Duration::from_secs(2)).await;
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the dropped route must not dial a callee whose caller is gone",
    );
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert_eq!(b2bua.metrics().decision_dropped_cancelled_total(), 1, "the drop is metered once",);

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // The CDR records the CANCEL and no trace of the dropped route.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Cancel),
        "the caller's CANCEL is on the CDR: {:?}",
        cdrs[0].events,
    );
    assert!(
        !cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::InviteSent),
        "no b-leg was ever dialed: {:?}",
        cdrs[0].events,
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
    assert_eq!(distinct_invite_call_ids(&report, bob.addr()), 0, "bob was never dialed");
}

/// Terminal-REJECT variant: the delayed 484 lands on the CANCELed call and is
/// dropped — the completed a-leg transaction never carries a second final.
#[tokio::test(start_paused = true)]
async fn reject_decision_landing_after_the_callers_cancel_is_dropped() {
    let h = Harness::new("reject-lands-on-cancelled-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(DelayedDecisionEngine {
        new_call_delay: DECISION_DELAY,
        failure_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_req| reject(484, "Address Incomplete"))
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(200)).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // Cross the decision's landing: the 484 is dropped, not authored — a
    // regression's second final surfaces here as an unexpected response
    // (`try_receive_tolerating` panics on any queued response).
    h.advance(Duration::from_secs(2)).await;
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert_eq!(b2bua.metrics().decision_dropped_cancelled_total(), 1);

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Cancel),
        "the caller's CANCEL is on the CDR: {:?}",
        cdrs[0].events,
    );
    assert!(
        !cdrs[0].events.iter().any(|e| e.status_code == Some(484)),
        "the dropped 484 leaves no reject on the CDR: {:?}",
        cdrs[0].events,
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
    assert_eq!(distinct_invite_call_ids(&report, bob.addr()), 0, "bob was never dialed");
}

/// Limiter variant: `apply_route` INCRs the route's `call_limiter` holds
/// BEFORE the drop seam runs, so the dropped result must still carry them onto
/// the resident call — the queued termination's obligation discharge DECRs
/// each one. A drop that discarded the holds would strand a cluster-visible
/// slot for the whole limiter window on exactly the caller-gives-up-early
/// path the spec calls common.
#[tokio::test(start_paused = true)]
async fn dropped_route_still_discharges_its_limiter_holds() {
    let h = Harness::new("dropped-route-limiter-discharge");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // A real `LimiterServer` on the simulated HTTP fabric (same rig as
    // `limiter.rs`), so the INCR is a genuine cluster hold we can probe.
    let laddr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr, server).await.unwrap();
    // Fail-open budget well above the paused-clock HTTP round trip: the 1 ms
    // simulated transits quantize to the 100 ms advance chunks the admit rides
    // through (it lands mid-`h.advance`, unlike the agent-pumped callflow
    // steps), so a production-sized 150 ms budget would fail open here.
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, Duration::from_secs(2)));

    let decision = Arc::new(DelayedDecisionEngine {
        new_call_delay: DECISION_DELAY,
        failure_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_req| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 10 }];
                    NewCallResponse::Route(r)
                })
                .build(),
        ),
    });
    let b2bua =
        B2buaSut::builder(decision).limiter(limiter).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(200)).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // Cross the decision's landing: the route INCRs its hold, then is dropped.
    h.advance(Duration::from_secs(2)).await;
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the dropped route must not dial a callee whose caller is gone",
    );
    assert_eq!(b2bua.metrics().decision_dropped_cancelled_total(), 1);

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // The admit really happened (its window key is live) AND the termination
    // discharged it — the INCR↔DECR pairing survives the drop.
    settle_until(|| store.stats().current_total == 0).await;
    let stats = store.stats();
    assert_eq!(stats.live_keys, 1, "the dropped route's admit INCRed a real hold");
    assert_eq!(stats.current_total, 0, "the termination DECRed the carried hold");

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// The route-supplied per-b-leg ring deadline for the fold-seam scenarios.
const NO_ANSWER_SEC: i64 = 5;

/// The fold-seam scenario frame shared by the failover ROUTE and REJECT
/// variants: bob rings then never answers, the `no-answer` consult parks on the
/// delayed backend, and the caller's CANCEL completes (200 + 487 + ACK) before
/// the fold lands on the now-Terminating call. Bob's own 487 for the b-leg
/// CANCEL is withheld until after the fold so the call is still resident when
/// it lands (a resolved call would already be released — the fold would hit the
/// orphan path, not the rule seam under test).
async fn fold_lands_on_terminating_call(
    name: &str,
    on_failure: impl Fn(&CallFailureRequest) -> CallTreatment + Send + Sync + 'static,
    consults: Arc<AtomicUsize>,
) -> (Harness, scenario_harness::Agent, scenario_harness::Agent, B2buaSut) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let inner = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut r = route_to("127.0.0.1", 5070);
            r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
            r.callback_context = Some("nk069".into());
            NewCallResponse::Route(r)
        })
        .on_failure(on_failure)
        .build();
    let decision = Arc::new(DelayedDecisionEngine {
        new_call_delay: Duration::ZERO,
        failure_delay: DECISION_DELAY,
        inner: Arc::new(inner),
    });
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    // ── established ring: alice INVITEs, bob rings and then goes silent ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── cross exactly the NoAnswer deadline ──────────────────────────────────
    // The consult parks on the delayed backend; DestroyLeg CANCELs the ringing
    // b-leg (the 180 lets it go on the wire). Bob 200s the CANCEL and WITHHOLDS
    // the 487.
    h.advance(Duration::from_secs(NO_ANSWER_SEC as u64)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;

    // ── the caller gives up while the consult is in flight ───────────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── cross the fold's landing on the still-Terminating call ───────────────
    h.advance(Duration::from_secs(2)).await;
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the consult itself was dispatched");
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert!(
        bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no fresh leg may be dialed toward a callee whose caller is gone",
    );

    // ── bob's withheld 487 resolves the cancelled b-leg; the call finalizes ──
    b_inv.respond(487, "Request Terminated").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    (h, alice, bob, b2bua)
}

/// Failover-ROUTE fold variant: the `failover` outcome landing on the
/// Terminating call is dropped — no replacement b-leg.
#[tokio::test(start_paused = true)]
async fn failover_route_fold_landing_after_the_callers_cancel_is_dropped() {
    let consults = Arc::new(AtomicUsize::new(0));
    let consults_in = consults.clone();
    let (h, alice, bob, _b2bua) = fold_lands_on_terminating_call(
        "failover-fold-cancelled-call",
        move |_req| {
            consults_in.fetch_add(1, Ordering::SeqCst);
            CallTreatment::Route(route_to("127.0.0.1", 5070))
        },
        consults,
    )
    .await;

    let report = h.finish().await;
    assert_eq!(
        distinct_invite_call_ids(&report, bob.addr()),
        1,
        "exactly the original b-leg INVITE reached bob — the dropped failover dialed nothing",
    );
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// Failover-REJECT fold variant: the decision-authored 480 landing on the
/// Terminating call is dropped — no second final on the completed transaction.
#[tokio::test(start_paused = true)]
async fn failover_reject_fold_landing_after_the_callers_cancel_is_dropped() {
    let consults = Arc::new(AtomicUsize::new(0));
    let consults_in = consults.clone();
    let (h, alice, bob, _b2bua) = fold_lands_on_terminating_call(
        "failover-reject-fold-cancelled-call",
        move |_req| {
            consults_in.fetch_add(1, Ordering::SeqCst);
            CallTreatment::Reject(RejectDecision {
                reject_code: 480,
                reject_reason: Some("Temporarily Unavailable".into()),
                update_headers: None,
            })
        },
        consults,
    )
    .await;

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
    assert_eq!(distinct_invite_call_ids(&report, bob.addr()), 1);
}

/// Control: the same delayed route on a call the caller does NOT cancel is
/// applied normally — the drop guard is scoped to the cancelled window, and a
/// CANCEL arriving after the route was applied keeps its ordinary treatment.
#[tokio::test(start_paused = true)]
async fn delayed_route_on_a_live_call_still_routes() {
    let h = Harness::new("delayed-route-live-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(DelayedDecisionEngine {
        new_call_delay: DECISION_DELAY,
        failure_delay: Duration::ZERO,
        inner: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
    });
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;
    assert_eq!(b2bua.metrics().decision_dropped_cancelled_total(), 0, "nothing was dropped");
    hangup(&mut dialog, &bob).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(distinct_invite_call_ids(&report, bob.addr()), 1, "the delayed route dialed bob");
}
