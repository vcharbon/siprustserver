//! The limiter holds an asynchronous route fold carries.
//!
//! A failover (`/call/failure`) or release-reroute consult runs detached from
//! the call: when the decision answers a route with call limiters, the
//! dispatching task admits them (all-or-none) BEFORE the fold reaches the
//! call. Those holds are then the call's to release, whatever became of the
//! call meanwhile:
//!
//!  - live call: the fold records them; the call's termination releases them;
//!  - going-away call (`Terminating`): the fold drives no progress, yet the
//!    holds join the call's ledger and its termination releases them;
//!  - gone call (evicted before the fold landed): the router releases them.
//!
//! Every scenario carries several limiters: distinct ids on one route, the
//! same id on the initial and the failover route (two holds on one
//! `(id, window)` are two releases), overlapping sets across a reroute. The
//! store is probed while the call holds them and drained to 0 after it ends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use call::ReleaseEventKind;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The in-flight window of a failover consult the caller's CANCEL lands in.
const FAILURE_DELAY: Duration = Duration::from_millis(900);
/// The in-flight window of a release consult the caller's BYE lands in —
/// inside the decision deadline (5 s).
const RELEASE_DELAY: Duration = Duration::from_secs(3);
/// The route-supplied ring deadline of the first b-leg.
const NO_ANSWER_SEC: i64 = 5;

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

/// A real `LimiterServer` on the simulated HTTP fabric, so every admit is a
/// genuine hold the store counts.
struct LimiterRig {
    store: Arc<WindowStore>,
    client: Arc<dyn CallLimiter>,
    _server: Box<dyn HttpServerHandle>,
}

async fn limiter_rig() -> LimiterRig {
    let laddr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = http.serve(laddr, server).await.unwrap();
    // A fail-open budget above the paused-clock HTTP round trip: the detached
    // admit is woken inside a coarse `h.advance`, whose 100 ms chunks a
    // production-sized budget could expire between.
    let client: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, Duration::from_secs(2)));
    LimiterRig { store, client, _server: handle }
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
/// lands. The initial route holds `x`; the failover route admits `x` + `y` —
/// the same id twice at one window, plus a distinct one. All three are
/// released once the call ends.
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
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;
    assert_eq!(rig.store.stats().current_total, 1, "the initial route holds x");

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
    let stats = rig.store.stats();
    assert_eq!(stats.live_keys, 2, "the failover admit INCRed x and y at the window");
    assert_eq!(stats.current_total, 3, "x twice (initial + failover) and y are held");

    // ── bob's withheld 487 resolves the b-leg; the call terminates ─────────
    b_inv.respond(487, "Request Terminated").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| rig.store.stats().current_total == 0).await;
    assert_eq!(
        rig.store.stats().current_total,
        0,
        "the termination releases the initial hold and both failover holds",
    );

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
/// (its own hold on `x` released) and is evicted before the fold lands. The
/// fold's admitted `x` + `y` have no call left to carry them: the router
/// releases them.
#[tokio::test(start_paused = true)]
async fn failover_fold_after_the_call_is_gone_releases_its_holds() {
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
    b2bua.assert_fully_reaped();
    settle_until(|| rig.store.stats().current_total == 0).await;
    assert_eq!(rig.store.stats().live_keys, 1, "only the initial route admitted so far");

    // ── the fold lands on the evicted call ─────────────────────────────────
    h.advance(Duration::from_secs(2)).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the fold dials no leg for a call that is gone",
    );
    settle_until(|| rig.store.stats().current_total == 0).await;
    let stats = rig.store.stats();
    assert_eq!(stats.live_keys, 2, "the failover admit INCRed y at the window");
    assert_eq!(stats.current_total, 0, "the router releases the gone call's fold holds");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
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
/// the call is still Terminating when the fold lands. The reroute admits `y` +
/// `z` (an overlapping set). Every hold is released once the call ends.
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
    assert_eq!(rig.store.stats().current_total, 2, "the established call holds x and y");

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
    let stats = rig.store.stats();
    assert_eq!(stats.live_keys, 3, "the reroute admit INCRed y and z at the window");
    assert_eq!(stats.current_total, 4, "x, y twice and z are held");

    // ── bob's 200 ends the call ─────────────────────────────────────────────
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| rig.store.stats().current_total == 0).await;
    assert_eq!(
        rig.store.stats().current_total,
        0,
        "the termination releases the route's holds and the reroute's",
    );

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// Control: the same failover fold on a live call records its holds — they
/// stay held while the rerouted call is up — and the call's hangup releases
/// them.
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
    let stats = rig.store.stats();
    assert_eq!(stats.live_keys, 2, "the failover admit INCRed x and y");
    assert_eq!(stats.current_total, 2, "the rerouted call holds x and y");

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| rig.store.stats().current_total == 0).await;
    assert_eq!(rig.store.stats().current_total, 0, "the hangup releases both failover holds");

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// A failover route whose SECOND limiter is at its cap: the all-or-none admit
/// refuses it and increments nothing — not even the first entry, which had
/// room. The refusal ends the chain in the stack's 486; the call's own hold
/// on `w` is released and the store drains to 0 with no key for `x` or `y`.
#[tokio::test(start_paused = true)]
async fn failover_route_refused_on_its_second_limiter_increments_nothing() {
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
                .fallback(|_| initial_route(5070, &["w"]))
                .on_failure(|_| {
                    let mut r = route_to("127.0.0.1", 5071);
                    r.call_limiter = vec![
                        CallLimiterEntry { id: "x".into(), limit: 10 },
                        CallLimiterEntry { id: "y".into(), limit: 0 },
                    ];
                    CallTreatment::Route(r)
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    assert_eq!(rig.store.stats().live_keys, 1, "the initial route holds w");

    // ── the refused failover ends the call in the stack's 486 ──────────────
    call.expect(486).await;
    assert!(
        carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the refused route dials nothing",
    );
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| rig.store.stats().current_total == 0).await;
    let stats = rig.store.stats();
    assert_eq!(stats.live_keys, 1, "the refused admit created no key for x nor y");
    assert_eq!(stats.current_total, 0, "the call's own hold on w is released");

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![486],
        "the a-leg INVITE transaction carries exactly one final: the 486",
    );
}
