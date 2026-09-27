//! The limiter set an asynchronous route fold settles for its call.
//!
//! A failover (`/call/failure`) or release-reroute consult runs detached from
//! the call: when the decision answers a route, the dispatching task replaces
//! the call's set on the limiter (one admit net of the set the call holds)
//! BEFORE the fold reaches the call. That set is then the call's to release,
//! whatever became of the call meanwhile:
//!
//!  - live call: the fold states it; the call's termination releases the call;
//!  - going-away call (`Terminating`): the fold drives no progress, yet it
//!    states the set and the call's termination releases the call;
//!  - gone call (released before the fold's admit landed): the admit meets the
//!    call's release fence and holds nothing.
//!
//! Every scenario carries several limiters: distinct ids on one route, the
//! same id on the initial and the failover route, overlapping sets across a
//! reroute. Each id carries one **witness** hold admitted under a call of its
//! own, so a surplus release reads below the witness. The store is probed per
//! id while the call holds its set and drained to the witnesses after it ends.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua_harness::{
    invite_final_statuses, settle_until, B2buaSut, WitnessRig, WITNESS_LIMITER_ADDR,
};
use call::ReleaseEventKind;
use call_limiter::LimiterConfig;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The in-flight window of a failover consult the caller's CANCEL lands in.
const FAILURE_DELAY: Duration = Duration::from_millis(900);
/// The in-flight window of a release consult the caller's BYE lands in —
/// inside the decision deadline (5 s).
const RELEASE_DELAY: Duration = Duration::from_secs(3);
/// The route-supplied ring deadline of the first b-leg.
const NO_ANSWER_SEC: i64 = 5;

/// The witness rig with a fail-open budget above the paused-clock HTTP round
/// trip (a detached admit is woken inside a coarse `h.advance`).
async fn limiter_rig() -> WitnessRig {
    WitnessRig::serve(LimiterConfig::default(), Duration::from_secs(2), None).await
}

/// Delay `call_failure` / `call_release` before delegating.
struct DelayedDecisionEngine {
    failure_delay: Duration,
    release_delay: Duration,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for DelayedDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
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
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        tokio::time::sleep(self.release_delay).await;
        self.inner.call_release(req).await
    }
}

fn limiters(ids: &[&str]) -> Vec<CallLimiterEntry> {
    ids.iter().map(|id| CallLimiterEntry { id: (*id).into(), limit: 10 }).collect()
}

/// The initial route: toward `port`, the ring deadline, a callback context
/// (so a failure consults the decision), and `ids` as its call limiters.
fn initial_route(port: u16, ids: &[&str]) -> NewCallResponse {
    let mut r = route_to("127.0.0.1", port);
    r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
    r.callback_context = Some("failover-ctx".into());
    r.call_limiter = limiters(ids);
    NewCallResponse::Route(r)
}

/// The failover route: toward `port`, with `ids` as its call limiters.
fn failover_route(port: u16, ids: &[&str]) -> CallTreatment {
    let mut r = route_to("127.0.0.1", port);
    r.call_limiter = limiters(ids);
    CallTreatment::Route(r)
}

/// Failover fold on a `Terminating` call. Bob rings and never answers; the
/// no-answer consult parks; the caller CANCELs; bob withholds the 487 of the
/// b-leg CANCEL so the call is still resident (Terminating) when the fold
/// lands. The initial route holds `x`; the failover route replaces it with
/// `x` + `y` — `x` kept, `y` added. The call is released once it ends.
#[tokio::test(start_paused = true)]
async fn failover_fold_on_a_terminating_call_releases_its_holds() {
    let h = Harness::new("failover-fold-holds-terminating-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: FAILURE_DELAY,
        release_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| initial_route(5070, &["x"]))
                .on_failure(|_| failover_route(5071, &["x", "y"]))
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;

    // ── the ring deadline: the consult parks, the ringing b-leg is CANCELed ──
    h.advance(Duration::from_secs(NO_ANSWER_SEC as u64)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;

    // ── the caller gives up while the consult is in flight ─────────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── the fold lands on the Terminating call ─────────────────────────────
    h.advance(Duration::from_secs(2)).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no leg toward a callee whose caller is gone",
    );
    rig.expect_holds([1, 1, 0], "the fold replaced the call's set: x kept, y added").await;

    // ── bob's withheld 487 resolves the b-leg; the call terminates ─────────
    b_inv.respond(487, "Request Terminated").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the termination releases the call's set").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// Failover fold on a call already gone. Bob busies out at once; the consult
/// parks; the caller CANCELs, which resolves the last leg: the call terminates
/// (released, its `x` freed) and is evicted before the fold's admit lands. The
/// admit meets the call's release fence: `x` + `y` are never counted, and the
/// fold has no call left to state them on.
#[tokio::test(start_paused = true)]
async fn failover_fold_after_the_call_is_gone_holds_nothing() {
    let h = Harness::new("failover-fold-holds-gone-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: FAILURE_DELAY,
        release_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| initial_route(5070, &["x"]))
                .on_failure(|_| failover_route(5071, &["x", "y"]))
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;

    // ── the caller gives up while the consult is in flight ─────────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_holds([0, 0, 0], "the terminated call released its set").await;
    b2bua.assert_calls_reaped();
    let refused_before = rig.store.stats().admits_refused_released;

    // ── the fold's admit lands on the evicted call's release fence ─────────
    h.advance(Duration::from_secs(2)).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no leg for a call that is gone",
    );
    assert_eq!(
        rig.store.stats().admits_refused_released,
        refused_before + 1,
        "the fold's admit was refused by the release fence",
    );
    rig.expect_drained("the gone call's fold counted nothing").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// Release-reroute fold on a `Terminating` call. The established call holds
/// `x` + `y`; its duration cap raises the subscribed release consult, which
/// parks; the caller hangs up and bob withholds his 200 to the relayed BYE so
/// the call is still Terminating when the fold lands. The reroute replaces the
/// set with `y` + `z` (an overlapping set). The call is released once it ends.
#[tokio::test(start_paused = true)]
async fn release_reroute_fold_on_a_terminating_call_releases_its_holds() {
    let h = Harness::new("release-reroute-fold-holds-terminating-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: Duration::ZERO,
        release_delay: RELEASE_DELAY,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.features.platform.max_duration_sec = 60;
                    r.callback_context = Some("release-ctx".into());
                    r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                    r.call_limiter = limiters(&["x", "y"]);
                    NewCallResponse::Route(r)
                })
                .on_release(|_| {
                    let mut r = route_to("127.0.0.1", 5090);
                    r.call_limiter = limiters(&["y", "z"]);
                    ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── establish A↔B ───────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the established call holds x and y").await;

    // ── the cap raises the release consult, which parks ─────────────────────
    h.advance(Duration::from_secs(61)).await;

    // ── the caller hangs up; bob withholds his 200 to the relayed BYE ───────
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;

    // ── the reroute fold lands on the Terminating call ─────────────────────
    h.advance(Duration::from_secs(RELEASE_DELAY.as_secs())).await;
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no replacement leg for a call whose parties hung up",
    );
    rig.expect_holds([0, 1, 1], "the reroute replaced the call's set: y kept, x freed, z added")
        .await;

    // ── bob's 200 ends the call ─────────────────────────────────────────────
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the termination releases the reroute's set").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// Control: the same failover fold on a live call states its set — held
/// while the rerouted call is up — and the call's hangup releases it.
#[tokio::test(start_paused = true)]
async fn failover_fold_on_a_live_call_records_and_releases_its_holds() {
    let h = Harness::new("failover-fold-holds-live-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: Duration::ZERO,
        release_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| initial_route(5070, &[]))
                .on_failure(|_| failover_route(5071, &["x", "y"]))
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let mut uas = carol.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the rerouted call holds x and y").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the call").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// A failover route whose SECOND limiter is at its cap: the all-or-none admit
/// refuses it and counts nothing — not even the first entry, which had room —
/// and releases the call's own `x` in the same step. The refusal ends the
/// chain in the stack's 486; the store drains to the witnesses.
#[tokio::test(start_paused = true)]
async fn failover_route_refused_on_its_second_limiter_counts_nothing() {
    let h = Harness::new("failover-fold-refused-second-limiter");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: Duration::ZERO,
        release_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| initial_route(5070, &["x"]))
                .on_failure(|_| {
                    let mut r = route_to("127.0.0.1", 5071);
                    // `z` at cap 1 is refused: its witness already holds one.
                    r.call_limiter = vec![
                        CallLimiterEntry { id: "y".into(), limit: 10 },
                        CallLimiterEntry { id: "z".into(), limit: 1 },
                    ];
                    CallTreatment::Route(r)
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the refused failover ends the call in the stack's 486 ──────────────
    call.expect(486).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the refused route dials nothing",
    );
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the refused admit counted nothing and released x").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (1, 1), "x granted, released by the refusal");
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![486],
        "the a-leg INVITE transaction carries exactly one final: the 486",
    );
}

/// Failover fold on a gone call that was uncounted at its end. The initial
/// route states no limiter; bob busies out; the consult parks; the caller
/// CANCELs and the call is evicted, releasing nothing (an uncounted call
/// leaves no release fence). The fold's admit of `x` + `y` is then granted: no
/// call is left to state it on, and the router releases the call.
#[tokio::test(start_paused = true)]
async fn failover_fold_after_an_uncounted_call_is_gone_is_released_by_the_router() {
    let h = Harness::new("failover-fold-uncounted-gone-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: FAILURE_DELAY,
        release_delay: Duration::ZERO,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| initial_route(5070, &[]))
                .on_failure(|_| failover_route(5071, &["x", "y"]))
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the caller gives up while the consult is in flight ─────────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_calls_reaped();
    let released_before = rig.store.stats().releases_total;

    // ── the fold's admit is granted; the router releases the gone call ─────
    h.advance(Duration::from_secs(2)).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no leg for a call that is gone",
    );
    rig.expect_drained("the router released the gone call's set").await;
    assert_eq!(
        rig.store.stats().releases_total,
        released_before + 1 + 3,
        "one release for the call"
    );
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (2, 2));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![487]);
}

/// Release-reroute fold whose admit fails open on a Terminating counted call:
/// the limiter is stalled when the fold's admit leaves, so the call stays
/// counted with its old set, and its terminal release frees that set.
#[tokio::test(start_paused = true)]
async fn a_fail_open_fold_on_an_ending_call_is_freed_by_its_release() {
    let h = Harness::new("release-reroute-fold-fail-open-terminating-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig =
        WitnessRig::serve(LimiterConfig { lease_sec: 20 }, Duration::from_millis(150), None).await;
    let decision = Arc::new(DelayedDecisionEngine {
        failure_delay: Duration::ZERO,
        release_delay: RELEASE_DELAY,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.features.platform.max_duration_sec = 60;
                    r.callback_context = Some("release-ctx".into());
                    r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                    r.call_limiter = limiters(&["x", "y"]);
                    NewCallResponse::Route(r)
                })
                .on_release(|_| {
                    let mut r = route_to("127.0.0.1", 5090);
                    r.call_limiter = limiters(&["y", "z"]);
                    ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the established call holds x and y").await;

    // ── the cap raises the release consult, which parks ─────────────────────
    for _ in 0..61 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    // ── the caller hangs up; the limiter stalls before the fold's admit ────
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    rig.http.apply_fault(http_net::Fault::Stall { dst: WITNESS_LIMITER_ADDR.parse().unwrap() });
    for _ in 0..RELEASE_DELAY.as_secs() {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no replacement leg for a call whose parties hung up",
    );
    rig.http.apply_fault(http_net::Fault::Resume { dst: WITNESS_LIMITER_ADDR.parse().unwrap() });

    // ── bob's 200 ends the call: still counted, it releases its set ────────
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_holds([0, 0, 0], "the terminal release freed the old set").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.failed_open, count.released), (1, 2));

    for _ in 0..22 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "the release freed the set");
    rig.expect_drained("released").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
