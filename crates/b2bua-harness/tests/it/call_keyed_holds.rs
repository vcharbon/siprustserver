//! A call's limiter holds are one set keyed by the call.
//!
//! A route fold (failover, release reroute) replaces the call's set in one
//! admit checked net of the set the call already holds: an id the call keeps
//! or reduces never refuses, only an id it adds is checked against its cap.
//! A refused replacement releases the call's old set (the ended leg is freed
//! before the failure is consulted). A call that ended before a fold's admit
//! lands holds nothing from it; an admit that failed open is never released.
//!
//! Every scenario carries several limiters with one **witness** hold per id,
//! admitted under a call of its own, so a surplus release reads below the
//! witness instead of vanishing under the store's floor at 0. The store is
//! probed per id while the call is up and drained to the witnesses after it
//! ends.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, RefreshOutcome};
use b2bua_harness::{
    invite_final_statuses, settle_until, B2buaSut, LimiterLeak, WitnessRig, WITNESS_IDS,
};
use call::ReleaseEventKind;
use call_limiter::LimiterConfig;
use http_net::{HttpRequest, HttpResponse, HttpService};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// A fail-open budget above the paused-clock HTTP round trip: a detached
/// admit is woken inside a coarse `h.advance`, whose 100 ms chunks a
/// production-sized budget could expire between.
const WIDE_BUDGET: Duration = Duration::from_secs(2);
/// A short lease, so the paused clock crosses it cheaply.
const LEASE_SEC: i64 = 20;

/// The witness rig under a short lease and a client budget of `budget`; the
/// server answers `answers_after` each request it applied.
async fn limiter_rig_with(budget: Duration, answers_after: Option<Duration>) -> WitnessRig {
    WitnessRig::serve(LimiterConfig { lease_sec: LEASE_SEC }, budget, answers_after).await
}

async fn limiter_rig(budget: Duration) -> WitnessRig {
    limiter_rig_with(budget, None).await
}

/// Limiter entries `(id, cap)`. A cap of 2 on an id the call already holds
/// once is exactly the witness plus the call: room for nothing new.
fn limiters(entries: &[(&str, i64)]) -> Vec<CallLimiterEntry> {
    entries.iter().map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit }).collect()
}

/// A route toward `host:port` with `entries` as its call limiters and a
/// callback context, so a failure of the leg it dials consults the decision.
fn limited_route(host: &str, port: u16, entries: &[(&str, i64)]) -> b2bua::decision::RouteDecision {
    let mut r = route_to(host, port);
    r.callback_context = Some("failover-ctx".into());
    r.call_limiter = limiters(entries);
    r
}

/// The initial route toward bob (5070) holding `initial`, whose failure
/// fails over to carol (5071) holding `failover`, whatever the failure's
/// origin: a limiter refusal of the failover route re-consults and is
/// refused again, which ends the chain in the stack's 486.
fn one_failover(
    initial: &'static [(&'static str, i64)],
    failover: &'static [(&'static str, i64)],
) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, initial)))
            .on_failure(move |_| CallTreatment::Route(limited_route("127.0.0.1", 5071, failover)))
            .build(),
    )
}

/// Bob busies out, the failover route is admitted, carol answers, alice
/// hangs up. `before` and `after` are the call's holds on [`WITNESS_IDS`] while bob
/// is dialed and once the failover route is applied.
async fn busy_then_failover_answered(
    initial: &'static [(&'static str, i64)],
    failover: &'static [(&'static str, i64)],
    name: &str,
    before: [i64; 3],
    after: [i64; 3],
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let b2bua = B2buaSut::builder(one_failover(initial, failover))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds(before, "the initial route's holds while bob is dialed").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route replaces the call's set: carol is dialed ────────
    rig.expect_holds(after, "the failover route is admitted net of the ids the call keeps").await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    rig.expect_holds(after, "the failover route's holds alone while carol rings").await;

    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    rig.expect_holds(after, "the failover route's holds alone while the call is up").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the failover route's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![200],
        "the a-leg INVITE transaction carries exactly one final: the 200",
    );
}

/// Initial `[x, y]`, failover `[y, z]` with `y` at its cap (the witness and
/// the call's own hold fill it): the call keeps `y`, so the replacement is
/// admitted and the call ends up holding `y` and `z` once each.
#[tokio::test(start_paused = true)]
async fn failover_route_keeping_an_id_at_its_cap_is_admitted() {
    busy_then_failover_answered(
        &[("x", 10), ("y", 10)],
        &[("y", 2), ("z", 10)],
        "keyed-holds-failover-kept-id-at-cap",
        [1, 1, 0],
        [0, 1, 1],
    )
    .await;
}

/// Initial `[x, x]`, failover `[x]` with `x` at its cap: the call reduces
/// `x`, which never refuses; it ends up holding `x` once.
#[tokio::test(start_paused = true)]
async fn failover_route_reducing_an_id_at_its_cap_is_admitted() {
    busy_then_failover_answered(
        &[("x", 10), ("x", 10)],
        &[("x", 2)],
        "keyed-holds-failover-reduced-id-at-cap",
        [2, 0, 0],
        [1, 0, 0],
    )
    .await;
}

/// Release reroute of an established call holding `[x, y]` toward a media
/// server with `[y, z]`, `y` at its cap: the call keeps `y`, so the reroute
/// is admitted and applied; the rerouted call holds `y` and `z`.
#[tokio::test(start_paused = true)]
async fn release_reroute_keeping_an_id_at_its_cap_is_admitted() {
    let h = Harness::new("keyed-holds-release-reroute-kept-id-at-cap");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_sec = 60;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(&[("y", 2), ("z", 10)]);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            // Inside the short lease, so the established call stays counted.
            c.limiter_refresh_sec = 5;
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

    // ── the cap raises the release consult; the reroute is admitted ────────
    // The witnesses are refreshed across the wait so their leases hold.
    for _ in 0..61 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.expect_holds([0, 1, 1], "the reroute is admitted net of the y the call keeps").await;
    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut retrans) = media.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;

    // The a-leg is re-INVITEd onto the media answer; the displaced b-leg is BYEd.
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    rig.expect_holds([0, 1, 1], "the rerouted call holds y and z").await;

    // ── the rerouted call ends normally ─────────────────────────────────────
    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// Delays the `n`-th `call_failure` (1-based) by `delay` before delegating.
struct DelayNthFailure {
    n: usize,
    delay: Duration,
    failures: AtomicUsize,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for DelayNthFailure {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        if self.failures.fetch_add(1, Ordering::SeqCst) + 1 == self.n {
            tokio::time::sleep(self.delay).await;
        }
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
        self.inner.call_release(req).await
    }
}

/// A refused replacement releases the call's old set. The initial route
/// holds `x`; bob busies out and the failover route `[y, z]` is refused on
/// its second entry (`z` at its cap): nothing is counted, not even `y`, and
/// the call's `x` is released in the same step, before the limiter-reject
/// re-consult answers. The re-consult's route `[z]` (room this time) is
/// applied; the call holds `z` alone and drains on hangup. The witnesses are
/// intact throughout.
#[tokio::test(start_paused = true)]
async fn refused_replacement_releases_the_call_holds() {
    let h = Harness::new("keyed-holds-refused-replacement");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let origins = Arc::new(Mutex::new(Vec::new()));
    let seen = origins.clone();
    let decision = Arc::new(DelayNthFailure {
        n: 2,
        delay: Duration::from_secs(1),
        failures: AtomicUsize::new(0),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    NewCallResponse::Route(limited_route("127.0.0.1", 5070, &[("x", 10)]))
                })
                .on_failure(move |req| {
                    seen.lock().unwrap().push(req.failure.origin.clone());
                    if req.failure.origin == "call_limiter" {
                        return CallTreatment::Route(limited_route(
                            "127.0.0.1",
                            5071,
                            &[("z", 10)],
                        ));
                    }
                    // `z` at cap 1 is refused: its witness already holds one.
                    CallTreatment::Route(limited_route("127.0.0.1", 5099, &[("y", 10), ("z", 1)]))
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
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route is refused; the re-consult is in flight ─────────
    h.advance(Duration::from_millis(500)).await;
    assert_eq!(
        *origins.lock().unwrap(),
        vec!["external".to_string()],
        "the failover route was answered; the re-consult is still delayed",
    );
    // Read at once: a settle would wait through the delayed re-consult.
    assert_eq!(
        rig.all_holds(),
        [0, 0, 0],
        "holds on {WITNESS_IDS:?}: the refused replacement released x; y and z were never counted",
    );

    // ── the re-consult's route is applied ───────────────────────────────────
    let mut carol_uas = carol.receive("INVITE").await;
    assert_eq!(
        *origins.lock().unwrap(),
        vec!["external".to_string(), "call_limiter".to_string()],
        "the refused route re-consulted with the limiter origin",
    );
    rig.expect_holds([0, 0, 1], "the applied route's z is the call's set").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the applied route's hold").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![200],
        "the a-leg INVITE transaction carries exactly one final: the 200",
    );
}

/// Delays every `call_failure` by `delay` before delegating.
struct DelayedFailure {
    delay: Duration,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for DelayedFailure {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        tokio::time::sleep(self.delay).await;
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
        self.inner.call_release(req).await
    }
}

/// The initial admit lands on the server but its answer comes past the
/// client's budget: the call runs uncounted, sends no refresh and no release,
/// and the server's count for it lapses with its lease. The witnesses' leases
/// are kept alive across it.
#[tokio::test(start_paused = true)]
async fn an_admit_that_times_out_and_lands_late_is_never_released() {
    let h = Harness::new("keyed-holds-late-admit-never-released");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // The limiter answers past the admit budget: the request lands, the
    // client gives up.
    let rig = limiter_rig_with(ADMIT_BUDGET, Some(Duration::from_millis(400))).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| c.limiter_refresh_sec = 5)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the late admit counted the call on the server").await;

    // Past several refresh periods the uncounted call has refreshed
    // nothing: its server-side set lapses with its lease.
    for _ in 0..LEASE_SEC {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    h.advance(Duration::from_secs(2)).await;
    rig.store.sweep_now();
    rig.expect_holds([0, 0, 0], "the uncounted call's set lapsed with its lease").await;
    assert_eq!(rig.store.stats().lease_expired_calls, 1);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the uncounted call released nothing").await;
    assert_eq!(rig.store.stats().releases_total, 3, "only the witnesses' releases");
    let count = b2bua.limiter_count();
    assert_eq!((count.failed_open, count.released), (1, 0));
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A failover fold whose admit reaches the server after the call ended:
/// the caller CANCELs while the consult is in flight, the call terminates
/// and releases its set; the fold's admit then lands on the release fence and
/// holds nothing; the gone-call path releases nothing. The witnesses are
/// intact.
#[tokio::test(start_paused = true)]
async fn a_fold_admitted_after_the_call_ended_holds_nothing() {
    let h = Harness::new("keyed-holds-fold-after-the-call-ended");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let decision = Arc::new(DelayedFailure {
        delay: Duration::from_millis(900),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.callback_context = Some("failover-ctx".into());
                    r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                    NewCallResponse::Route(r)
                })
                .on_failure(|_| {
                    let mut r = route_to("127.0.0.1", 5071);
                    r.call_limiter = limiters(&[("y", 10), ("z", 10)]);
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
    rig.expect_holds([1, 1, 0], "the initial route's set").await;

    // ── the caller gives up while the consult is in flight ─────────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_holds([0, 0, 0], "the terminated call released its set").await;
    let refused_before = rig.store.stats().admits_refused_released;

    // ── the fold lands on the gone call: its admit hit the release fence
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
    rig.expect_drained("the fold's set was never counted; the witnesses are intact").await;
    assert_eq!(b2bua.metrics().limiter_admit_released_fold_total(), 1);
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![487]);
}

/// A route holding `entries` toward bob, with no failover.
fn routes_holding(entries: &'static [(&'static str, i64)]) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = limiters(entries);
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The limiter key is unique over time. A retried INVITE reusing the first
/// call's Call-ID and From tag (RFC 3261 §8.1.3.5) is a call of its own on
/// the limiter: it holds `x` although the first call's release fenced
/// their common `call_ref`, and a third call on `x` at its cap is refused.
#[tokio::test(start_paused = true)]
async fn a_retried_invite_reusing_the_call_identity_is_counted() {
    const CALL_ID: &str = "retried-call@127.0.0.1";
    const FROM_TAG: &str = "retried-from-tag";
    let h = Harness::new("keyed-holds-retried-identity");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    // x at cap 2: the witness and one call.
    let b2bua = B2buaSut::builder(routes_holding(&[("x", 2)]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── the first call is challenged and ends ──────────────────────────────
    let mut first = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    bob.receive("INVITE")
        .await
        .respond(401, "Unauthorized")
        .with_header("WWW-Authenticate", "Digest realm=\"bob\", nonce=\"n1\"")
        .await;
    bob.receive("ACK").await;
    first.expect(401).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_holds([0, 0, 0], "the challenged call released its set").await;

    // ── the retry under the same identity is a counted call ────────────────
    let mut retry = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(2)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    retry.expect(200).await;
    let mut dialog = retry.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the retried call holds x").await;

    // ── x is at its cap: a third call is refused ───────────────────────────
    let mut third = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    third.expect(486).await;
    rig.expect_holds([1, 0, 0], "the refused call counted nothing").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the retried call").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The limiter restarts with counted calls live: its store is empty, and each
/// call's next refresh re-registers its set (no cap check), so the counts are
/// back within one refresh period. The hangups release once.
#[tokio::test(start_paused = true)]
async fn a_limiter_restart_is_healed_by_the_next_refresh() {
    let h = Harness::new("keyed-holds-limiter-restart");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut rig = limiter_rig(WIDE_BUDGET).await;
    let b2bua = B2buaSut::builder(routes_holding(&[("x", 10), ("y", 10)]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| c.limiter_refresh_sec = 5)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialogs = Vec::new();
    for caller in [&alice, &carol] {
        let mut call = caller.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
        bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        dialogs.push(call.ack().await);
        bob.receive("ACK").await;
    }
    rig.expect_holds([2, 2, 0], "two counted calls").await;

    // ── the limiter restarts empty ─────────────────────────────────────────
    let dead = rig.restart().await;
    assert_eq!(rig.all_holds(), [0, 0, 0], "the restarted store holds nothing for the calls");
    h.advance(Duration::from_secs(6)).await;
    rig.expect_holds([2, 2, 0], "each call's refresh re-registered its set").await;
    assert_eq!(rig.store.stats().reregistered_calls, 2);
    assert_eq!(b2bua.metrics().limiter_refresh_reregistered_total(), 2);

    for mut dialog in dialogs {
        let mut bye = dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
    }
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangups release the re-registered sets once").await;
    assert_eq!(rig.store.stats().releases_total, 5, "two calls and three witnesses");
    // The SUT's ledger saw one grant and one release per call; the store it
    // reads is the dead one, frozen with the two calls and the witnesses.
    b2bua.assert_fully_reaped_leaving(LimiterLeak {
        unreleased: 0,
        stored: dead.stats().current_total,
    });
    assert_eq!(dead.stats().current_total, 7);
    let _ = h.finish().await;
}

/// The limiter applies a `/v1/refresh` `after` it arrived, detached from the
/// request: a client past its budget has given up when it lands.
struct LateRefresh {
    inner: Arc<dyn HttpService>,
    after: Duration,
}

#[async_trait]
impl HttpService for LateRefresh {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        if req.path != "/v1/refresh" {
            return self.inner.handle(req).await;
        }
        let (inner, after) = (self.inner.clone(), self.after);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(inner.handle(req).await);
        });
        rx.await.unwrap_or_else(|_| HttpResponse::status(500))
    }
}

/// A refresh that left before a refused reroute dropped the call's set and
/// lands after it re-creates nothing: the drop fenced the key. The call holds
/// `x`; its refresh is applied late; the release reroute `[y, z]` is refused
/// on `z` (at its cap) and drops `x`; the late refresh then lands on the fence.
/// The call ends by the local teardown, drained.
#[tokio::test(start_paused = true)]
async fn a_refresh_landing_after_a_refused_reroute_dropped_the_set_re_creates_nothing() {
    let h = Harness::new("keyed-holds-late-refresh-after-drop");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = WitnessRig::serve_wrapped(LimiterConfig { lease_sec: LEASE_SEC }, WIDE_BUDGET, |s| {
        Arc::new(LateRefresh { inner: s, after: Duration::from_secs(5) })
    })
    .await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = limited_route("127.0.0.1", 5070, &[("x", 10)]);
                r.features.platform.max_duration_sec = 60;
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                // z at cap 1 is refused: its witness already holds one.
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(&[("y", 10), ("z", 1)]);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
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
    let _dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── every refresh is applied 5 s late; the cap raises the reroute ──────
    // The refresh of t = 60 s is in flight when the refused reroute's admit
    // drops x; it lands 5 s later on the fence.
    for _ in 0..61 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    // The refused reroute degrades to the local teardown of both legs.
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    for _ in 0..8 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert_eq!(rig.all_holds(), [0, 0, 0], "the late refresh re-created nothing");
    assert_eq!(rig.store.stats().reregistered_calls, 0);
    rig.expect_drained("nothing of the call is held").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (1, 1), "x granted, released by the refusal");
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A `LimiterRefresh` fire the driver lost (a full per-call queue drops it):
/// the set lapses, and the call's next turn re-arms the refresh, which
/// re-registers the set. Nothing but the turn heals it.
#[tokio::test(start_paused = true)]
async fn a_dropped_refresh_fire_is_re_armed_by_the_call_s_next_turn() {
    const CALL_ID: &str = "dropped-fire@127.0.0.1";
    const FROM_TAG: &str = "dropped-fire-tag";
    let h = Harness::new("keyed-holds-dropped-refresh-fire");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let b2bua = B2buaSut::builder(routes_holding(&[("x", 10)]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── the driver loses the refresh fire; the set lapses ──────────────────
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    b2bua.cancel_driver_timer(&call_ref, "LimiterRefresh").await;
    for _ in 0..22 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.all_holds(), [0, 0, 0], "nobody refreshed the set");
    assert_eq!(rig.store.stats().lease_expired_calls, 1);

    // ── the call's next turn re-arms the refresh, which re-registers ───────
    let mut probe = dialog.request(InDialogMethod::Options, None).await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;
    let _ = probe.expect(200).await;
    for _ in 0..6 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.expect_holds([1, 0, 0], "the re-armed refresh re-registered the set").await;
    assert_eq!(rig.store.stats().reregistered_calls, 1);
    assert_eq!(b2bua.metrics().limiter_refresh_reregistered_total(), 1);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the re-registered set").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A counted call whose key the limiter released behind its back (a
/// partitioned backup's reap of a primary it believes dead): its refreshes are
/// refused for one lease and counted, then the next one re-registers the set.
/// The hangup releases once.
#[tokio::test(start_paused = true)]
async fn a_refresh_refused_by_a_release_behind_the_call_s_back_is_counted() {
    const CALL_ID: &str = "released-behind@127.0.0.1";
    const FROM_TAG: &str = "released-behind-tag";
    let h = Harness::new("keyed-holds-refresh-released");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = limiter_rig(WIDE_BUDGET).await;
    let b2bua = B2buaSut::builder(routes_holding(&[("x", 10)]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── the key is released behind the call's back ─────────────────────────
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let key = b2bua.live_call(&call_ref).expect("the call is live").limiter.key;
    rig.store.release(&key);
    assert_eq!(rig.all_holds(), [0, 0, 0]);
    for _ in 0..6 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert_eq!(rig.all_holds(), [0, 0, 0], "the release fence refuses the refresh");
    assert_eq!(b2bua.metrics().limiter_refresh_released_total(), 1);

    // ── past the fence's lease the refresh re-registers ────────────────────
    for _ in 0..LEASE_SEC + 5 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.expect_holds([1, 0, 0], "the set is re-registered once the fence lapsed").await;
    assert_eq!(b2bua.metrics().limiter_refresh_reregistered_total(), 1);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the re-registered set").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The path of every request the limiter server received, in arrival order.
struct RequestLog {
    inner: Arc<dyn HttpService>,
    paths: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl HttpService for RequestLog {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.paths.lock().unwrap().push(req.path.clone());
        self.inner.handle(req).await
    }
}

/// The witness rig behind a [`RequestLog`]; returns the log.
async fn logged_rig() -> (WitnessRig, Arc<Mutex<Vec<String>>>) {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let log = paths.clone();
    let rig =
        WitnessRig::serve_wrapped(LimiterConfig { lease_sec: LEASE_SEC }, WIDE_BUDGET, move |s| {
            Arc::new(RequestLog { inner: s, paths: log.clone() })
        })
        .await;
    (rig, paths)
}

/// The requests the limiter received after its last admit.
fn after_last_admit(paths: &Mutex<Vec<String>>) -> Vec<String> {
    let paths = paths.lock().unwrap();
    let last = paths.iter().rposition(|p| p == "/v1/admit").expect("an admit was received");
    paths[last + 1..].to_vec()
}

/// A refused release reroute leaves the call uncounted: the refusal dropped
/// the call's `x`, so the call sends no refresh and no terminal release after
/// it. The call holds `x`; the release reroute `[y, z]` is refused on `z`
/// (at its cap); the call ends by the local teardown, drained.
#[tokio::test(start_paused = true)]
async fn a_refused_release_reroute_leaves_the_call_uncounted() {
    let h = Harness::new("keyed-holds-refused-reroute-uncounted");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = logged_rig().await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = limited_route("127.0.0.1", 5070, &[("x", 10)]);
                // Off the refresh period's multiples: no refresh is in flight
                // when the reroute is admitted.
                r.features.platform.max_duration_sec = 62;
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                // z at cap 1 is refused: its witness already holds one.
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(&[("y", 10), ("z", 1)]);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
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
    let _dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── the cap raises the reroute; its admit is refused and drops x ───────
    for _ in 0..62 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    for _ in 0..8 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }

    assert_eq!(
        after_last_admit(&paths),
        Vec::<String>::new(),
        "the refused reroute left the call uncounted: no refresh, no release after it",
    );
    assert_eq!(rig.store.stats().releases_total, 0, "no terminal release");
    assert_eq!(b2bua.metrics().limiter_refresh_released_total(), 0);
    rig.expect_drained("the refusal dropped the call's set").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (1, 1), "x granted, released by the refusal");
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// The failure chain's terminal 486 leaves the call uncounted: the refused
/// failover route dropped the call's `x`, and the re-consult is refused again,
/// so the call ends with no terminal release. Bob busies out; the failover
/// route `[y, z]` is refused on `z` (at its cap) at every depth.
#[tokio::test(start_paused = true)]
async fn the_failure_chain_s_terminal_486_leaves_the_call_uncounted() {
    let h = Harness::new("keyed-holds-terminal-486-uncounted");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = logged_rig().await;
    let b2bua = B2buaSut::builder(one_failover(&[("x", 10)], &[("y", 10), ("z", 1)]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    call.expect(486).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;

    assert_eq!(
        after_last_admit(&paths),
        Vec::<String>::new(),
        "the refused chain left the call uncounted: no release after it",
    );
    assert_eq!(rig.store.stats().releases_total, 0, "no terminal release");
    rig.expect_drained("the first refusal dropped the call's set").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (1, 1), "x granted, released by the refusal");
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![486]);
}

/// A limiter that answers every admit `Released`: the release-fence refusal on
/// an initial route leaves the call uncounted and is counted on the b2bua.
struct AnswersReleased;

#[async_trait]
impl CallLimiter for AnswersReleased {
    async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
        AdmitOutcome::Released
    }
    async fn release(&self, _key: &str) {}
    async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
        RefreshOutcome::Released
    }
}

/// An initial admit refused by a release fence runs the call uncounted and counts
/// it as `limiter_admit_released_initial`.
#[tokio::test(start_paused = true)]
async fn an_initial_admit_refused_by_a_release_fence_runs_the_call_uncounted() {
    let h = Harness::new("keyed-holds-initial-admit-released");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(routes_holding(&[("x", 10)]))
        .limiter(Arc::new(AnswersReleased))
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.metrics().limiter_admit_released_initial_total(), 1);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (0, 0, 0), "uncounted");
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
