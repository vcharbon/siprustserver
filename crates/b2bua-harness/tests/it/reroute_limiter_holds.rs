//! A reroute's limiter set replaces the replaced route's.
//!
//! The limiter set of a call belongs to its latest applied route. When a
//! failover route or a release reroute is applied, its dispatching task has
//! replaced the call's set on the limiter in one admit: the replaced route's
//! holds are freed and the new route's become the call's. A route that states
//! no limiter still replaces: the call is then uncounted. A route whose admit
//! fails technically leaves a counted call counted: it keeps refreshing
//! whichever set the limiter holds for it (the old one, or the new one when
//! the admit landed) and its terminal release frees that set.
//!
//! Every scenario carries several limiters: distinct ids on one route, the
//! same id on the replaced and the new route, the same id twice, overlapping
//! sets. Each id carries one **witness** hold admitted under a call of its
//! own, so a surplus release shows as a count below the witness instead of
//! vanishing under the store's floor at 0. The store is probed per id while
//! the call is up and drained to the witnesses after it ends; the witnesses
//! are then released, so the reaped check reads the store empty.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, CallTreatment,
    NewCallRequest, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua::limiter::RefreshOutcome;
use b2bua::limiter::{CallLimiter, LimiterEntry};
use b2bua_harness::limiter::doubles::{spy, unavailable_on_nth, Spy};
use b2bua_harness::{
    invite_final_statuses, settle_until, B2buaSut, LimiterLeak, WitnessRig, WITNESS_IDS,
};
use call::ReleaseEventKind;
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, LimiterConfig};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The witness rig under `cfg`, with a 150 ms fail-open budget.
async fn limiter_rig_with(cfg: LimiterConfig) -> WitnessRig {
    WitnessRig::serve(cfg, Duration::from_millis(150), None).await
}

async fn limiter_rig() -> WitnessRig {
    limiter_rig_with(LimiterConfig::default()).await
}

/// The admits `spy` saw that replace a call's set (`release_on_refusal`: a
/// route fold's).
fn replacing(spy: &Spy) -> usize {
    spy.admits().iter().filter(|a| a.release_on_refusal).count()
}

fn limiters(ids: &[&str]) -> Vec<LimiterEntry> {
    ids.iter().map(|id| LimiterEntry { id: (*id).into(), limit: 10 }).collect()
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

/// A route toward `host:port` with `ids` as its call limiters and a callback
/// context, so a failure of the leg it dials consults the decision again.
fn limited_route(host: &str, port: u16, ids: &[&str]) -> b2bua::decision::RouteDecision {
    let mut r = route_to(host, port);
    r.callback_context = Some("failover-ctx".into());
    r.call_limiter = limiters(ids);
    r
}

/// The initial route toward bob (5070) holding `initial`, whose failure fails
/// over to carol (5071) holding `failover`.
fn one_failover(
    initial: &'static [&'static str],
    failover: &'static [&'static str],
) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, initial)))
            .on_failure(move |_| CallTreatment::Route(limited_route("127.0.0.1", 5071, failover)))
            .build(),
    )
}

/// Bob busies out, carol rings then answers, alice hangs up: the failover
/// shape every single-failover scenario shares. `before` and `after` are the
/// call's expected holds on [`WITNESS_IDS`] while bob is dialed and once the failover
/// route is applied. Returns the SUT and the rig once the call is reaped, for
/// the caller's own drain and reaped checks.
async fn busy_then_failover_answered(
    initial: &'static [&'static str],
    failover: &'static [&'static str],
    rig: WitnessRig,
    limiter: Arc<dyn CallLimiter>,
    name: &str,
    before: [i64; 3],
    after: [i64; 3],
) -> (B2buaSut, WitnessRig) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(one_failover(initial, failover))
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds(before, "the initial route's holds while bob is dialed").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route is applied: its set replaces the initial one ───
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
    settle_until(|| b2bua.is_reaped()).await;
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
    (b2bua, rig)
}

/// The shape above, drained: the hangup releases the failover route's set.
async fn busy_then_failover_answered_drained(
    initial: &'static [&'static str],
    failover: &'static [&'static str],
    name: &str,
    before: [i64; 3],
    after: [i64; 3],
) {
    let rig = limiter_rig().await;
    let limiter = rig.client.clone();
    let (b2bua, rig) =
        busy_then_failover_answered(initial, failover, rig, limiter, name, before, after).await;
    rig.expect_drained("the hangup releases the failover route's holds").await;
    b2bua.assert_fully_reaped();
}

/// Initial `[x, y]`, failover `[y, z]` (an overlapping set): once the failover
/// is applied the call holds `y` and `z` once each and nothing on `x`.
#[tokio::test(start_paused = true)]
async fn failover_route_replaces_the_initial_route_holds() {
    busy_then_failover_answered_drained(
        &["x", "y"],
        &["y", "z"],
        "reroute-holds-failover-overlapping",
        [1, 1, 0],
        [0, 1, 1],
    )
    .await;
}

/// Initial `[x, x]` (the same id twice), failover `[x]`: the call reduces
/// `x` and holds it once.
#[tokio::test(start_paused = true)]
async fn failover_route_on_the_same_id_holds_it_once() {
    busy_then_failover_answered_drained(
        &["x", "x"],
        &["x"],
        "reroute-holds-failover-same-id",
        [2, 0, 0],
        [1, 0, 0],
    )
    .await;
}

/// A failover route stating no limiter replaces `[x, y]` with nothing: the
/// call is uncounted from then on.
#[tokio::test(start_paused = true)]
async fn failover_route_without_limiters_releases_the_initial_route_holds() {
    busy_then_failover_answered_drained(
        &["x", "y"],
        &[],
        "reroute-holds-failover-unlimited",
        [1, 1, 0],
        [0, 0, 0],
    )
    .await;
}

/// The lease of the failed-reroute scenarios: shorter than the call lasts.
const SHORT_LEASE_SEC: i64 = 20;

/// Initial `[x]`, bob busies out, and the failover route `[y, z]`'s admit
/// fails technically (`lands`: it reached the limiter, its answer is lost).
/// The call stays counted: carol answers, the call lasts longer than two
/// leases while refreshing every 5 s, and `held` is what the limiter holds
/// for it throughout — the old `[x]`, or the landed `[y, z]`. No set lapses;
/// the hangup's release drains the call.
async fn failover_admit_fails_and_the_call_outlives_the_lease(
    name: &str,
    lands: bool,
    held: [i64; 3],
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig_with(LimiterConfig { lease_sec: SHORT_LEASE_SEC }).await;
    let limiter = unavailable_on_nth(2, lands, rig.client.clone());
    let b2bua = B2buaSut::builder(one_failover(&["x"], &["y", "z"]))
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route's x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    rig.expect_holds(held, "the set the limiter holds for the call").await;
    let gauge = || b2bua.metrics().limiter().uncounted_calls();
    assert_eq!(gauge(), 1, "counted on [x], the call runs open on the route's [y, z]");

    // ── the call outlives two leases: its refresh keeps the set alive ─────
    for _ in 0..2 * SHORT_LEASE_SEC + 5 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.all_holds(), held, "the refreshed set outlives its lease");
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "no set lapsed");
    assert_eq!(
        gauge(),
        if lands { 0 } else { 1 },
        "a refresh answer stating the landed set repairs the lost answer; nothing else does"
    );

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup's release frees the set the limiter holds").await;
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "freed by its release, not its lease");
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (1, 1, 1));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![200]);
}

/// The failover admit never reaches the limiter: the call keeps its `[x]`,
/// refreshed, until its release.
#[tokio::test(start_paused = true)]
async fn failover_admit_that_fails_before_landing_keeps_the_call_counted() {
    failover_admit_fails_and_the_call_outlives_the_lease(
        "reroute-holds-failover-admit-lost",
        false,
        [1, 0, 0],
    )
    .await;
}

/// The failover admit lands and its answer is lost: the limiter holds
/// `[y, z]` for the call, which the call's refresh (naming `[x]`) extends and
/// its release frees.
#[tokio::test(start_paused = true)]
async fn failover_admit_that_lands_but_times_out_keeps_the_call_counted() {
    failover_admit_fails_and_the_call_outlives_the_lease(
        "reroute-holds-failover-admit-answer-lost",
        true,
        [0, 1, 1],
    )
    .await;
}

/// The initial admit of `[x]` gets no answer (it never lands): the call runs
/// uncounted while bob is dialed. Bob busies out and the failover route
/// `failover` is admitted: the call is counted on it, or asks for nothing on
/// a route naming no id; either way it no longer fails open. The call's end
/// releases its key and leaves nothing behind.
async fn a_call_failing_open_then_failing_over(
    name: &str,
    failover: &'static [&'static str],
    after: [i64; 3],
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let limiter = unavailable_on_nth(1, false, rig.client.clone());
    let b2bua = B2buaSut::builder(one_failover(&["x"], failover))
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let gauge = || b2bua.metrics().limiter().uncounted_calls();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([0, 0, 0], "the initial admit never landed").await;
    assert_eq!(gauge(), 1, "the call runs uncounted on a route naming x");
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    rig.expect_holds(after, "the failover route's set").await;
    assert_eq!(gauge(), 0, "the failover route ends the fail-open");

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup's release frees the failover set").await;
    assert_eq!(gauge(), 0);
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![200]);
}

#[tokio::test(start_paused = true)]
async fn a_failover_route_counts_a_call_that_failed_open() {
    a_call_failing_open_then_failing_over(
        "reroute-holds-fail-open-then-counted",
        &["y", "z"],
        [0, 1, 1],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_failover_route_naming_no_limiter_ends_the_fail_open() {
    a_call_failing_open_then_failing_over(
        "reroute-holds-fail-open-then-no-limiter",
        &[],
        [0, 0, 0],
    )
    .await;
}

/// Two consecutive failovers `[x]` → `[y]` → `[z]`: at each point only the
/// latest applied route's hold is held.
#[tokio::test(start_paused = true)]
async fn consecutive_failovers_hold_only_the_latest_route() {
    let h = Harness::new("reroute-holds-consecutive-failovers");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let dave = h.agent("dave", "127.0.0.1:5072").await;
    let rig = limiter_rig().await;
    let failures = Arc::new(AtomicUsize::new(0));
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, &["x"])))
            .on_failure(move |_| match failures.fetch_add(1, Ordering::SeqCst) {
                0 => CallTreatment::Route(limited_route("127.0.0.1", 5071, &["y"])),
                _ => CallTreatment::Route(limited_route("127.0.0.1", 5072, &["z"])),
            })
            .build(),
    );
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

    let mut carol_uas = carol.receive("INVITE").await;
    rig.expect_holds([0, 1, 0], "the first failover holds y alone").await;
    carol_uas.respond(503, "Service Unavailable").await;
    carol.receive("ACK").await;

    let mut dave_uas = dave.receive("INVITE").await;
    rig.expect_holds([0, 0, 1], "the second failover holds z alone").await;
    dave_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    dave.receive("ACK").await;

    let mut bye = dialog.bye().await;
    dave.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the latest route's hold").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// A refused replacement route releases the call's set. The initial route
/// holds `x`; bob busies out and the failover route `[y, z]` is refused on its
/// second entry (`z` at its cap): nothing is counted, not even `y`, and the
/// call's `x` is released in the same step, while the limiter-reject
/// re-consult is still in flight. The re-consult's route `[z]` (room this
/// time) is applied as the call's set.
#[tokio::test(start_paused = true)]
async fn refused_failover_route_releases_the_call_holds_before_the_re_consult() {
    let h = Harness::new("reroute-holds-refused-failover");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let origins = Arc::new(Mutex::new(Vec::new()));
    let seen = origins.clone();
    let decision = Arc::new(DelayNthFailure {
        n: 2,
        delay: Duration::from_secs(1),
        failures: AtomicUsize::new(0),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, &["x"])))
                .on_failure(move |req| {
                    seen.lock().unwrap().push(req.failure.origin.clone());
                    if req.failure.origin == "call_limiter" {
                        return CallTreatment::Route(limited_route("127.0.0.1", 5071, &["z"]));
                    }
                    // `z` at cap 1 is refused (its witness already holds one).
                    let mut r = limited_route("127.0.0.1", 5099, &[]);
                    r.call_limiter = vec![
                        LimiterEntry { id: "y".into(), limit: 10 },
                        LimiterEntry { id: "z".into(), limit: 1 },
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
        "holds on {WITNESS_IDS:?}: the refused route counted nothing and released x",
    );

    // ── the re-consult's route is applied as the call's set ────────────────
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
    settle_until(|| b2bua.is_reaped()).await;
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

/// The limiter restarts empty while bob is dialed, and calls admitted since
/// take `x` to its cap of 10 beside the witness. Bob busies out; the failover
/// route `[x, y]` is admitted before any refresh of the call (a ringing call
/// does not refresh): its admit carries the call's held `[x]`, which the
/// restarted limiter re-registers, so `x` is kept, not added, and only `y`
/// is checked. The hangup's release frees the failover route's set.
#[tokio::test(start_paused = true)]
async fn failover_admit_reaching_a_restarted_limiter_keeps_the_call_s_own_ids() {
    let h = Harness::new("reroute-holds-failover-restarted-limiter");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let mut rig = limiter_rig().await;
    let b2bua = B2buaSut::builder(one_failover(&["x"], &["x", "y"]))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;

    // ── the limiter restarts empty; x reaches its cap without the call ─────
    let dead = rig.restart().await;
    let since = vec![AdmitEntry { id: "x".into(), limit: 100 }; 9];
    assert_eq!(rig.store.admit("since", 1, &since, false), AdmitResult::Admitted);
    assert_eq!(rig.store.held("x"), 10, "x at its cap of 10: the witness and the calls since");

    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    rig.expect_holds([10, 1, 0], "the failover kept the call's x and added y").await;
    assert_eq!(rig.store.stats().admit_reregistered_calls, 1);

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_holds([9, 0, 0], "the hangup released the call's set").await;
    rig.store.release(&["since"]);
    rig.expect_drained("the calls since released theirs").await;
    // The SUT's ledger reads the dead store, frozen with the initial route's
    // set and the witnesses.
    assert_eq!(dead.stats().current_total, 4);
    b2bua.assert_fully_reaped_leaving(LimiterLeak {
        unreleased: 0,
        stored: dead.stats().current_total,
        queued: Vec::new(),
    });
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// A call holding `[x]` whose failover route `[x, y]` is refused on `y` (its
/// witness fills its cap of 1), the refusal dropping the call's set.
/// `second` is the limiters of the re-consult's route toward carol, `third`
/// those of the route after a second refusal. Returns the decision and the
/// failure origins it saw.
fn refused_then(
    second: Vec<LimiterEntry>,
    third: Option<Vec<LimiterEntry>>,
) -> (Arc<ScriptedDecisionEngine>, Arc<Mutex<Vec<String>>>) {
    let origins = Arc::new(Mutex::new(Vec::new()));
    let seen = origins.clone();
    let decision = ScriptedDecisionEngine::builder()
        .fallback(|_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, &["x"])))
        .on_failure(move |req| {
            let mut seen = seen.lock().unwrap();
            seen.push(req.failure.origin.clone());
            let mut r = limited_route("127.0.0.1", 5071, &[]);
            r.call_limiter = match seen.len() {
                1 => {
                    r.destination.port = Some(5099);
                    vec![
                        LimiterEntry { id: "x".into(), limit: 10 },
                        LimiterEntry { id: "y".into(), limit: 1 },
                    ]
                }
                2 => second.clone(),
                _ => third.clone().unwrap_or_default(),
            };
            CallTreatment::Route(r)
        })
        .build();
    (Arc::new(decision), origins)
}

/// A refused failover route dropped the call's set: the re-consult's route
/// states no limiter, and the call, holding nothing, sends no admit for it
/// (the re-consult carries the empty set the refusal stated, not the `[x]`
/// the consult was dispatched with).
#[tokio::test(start_paused = true)]
async fn a_re_consult_after_a_refusal_carries_the_set_the_refusal_stated() {
    let h = Harness::new("reroute-holds-re-consult-carries-the-refusal");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let limiter = spy(rig.client.clone());
    let (decision, origins) = refused_then(Vec::new(), None);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    assert_eq!(*origins.lock().unwrap(), ["external", "call_limiter"]);
    assert_eq!(
        replacing(&limiter),
        1,
        "the refused failover route's admit alone: a call holding nothing sends none for a \
         route naming no limiter"
    );
    rig.expect_holds([0, 0, 0], "the refusal dropped the call's set").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("nothing is held for the call").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The limiter restarts between a refused failover route and the
/// re-consult's admit, losing the drop fence: the re-consult's route `[x]`
/// (its witness fills its cap of 1) carries the empty set the refusal
/// stated, so the restarted limiter re-creates nothing and checks `x` as
/// added, refusing it. The next re-consult's route states no limiter.
#[tokio::test(start_paused = true)]
async fn a_re_consult_reaching_a_restarted_limiter_re_creates_nothing_the_refusal_dropped() {
    let h = Harness::new("reroute-holds-re-consult-restarted-limiter");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let mut rig = limiter_rig().await;
    let limiter = spy(rig.client.clone());
    let (scripted, origins) =
        refused_then(vec![LimiterEntry { id: "x".into(), limit: 1 }], Some(Vec::new()));
    let decision = Arc::new(DelayNthFailure {
        n: 2,
        delay: Duration::from_secs(1),
        failures: AtomicUsize::new(0),
        inner: scripted,
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route is refused; the limiter restarts while the
    //    re-consult is in flight ─────────────────────────────────────────────
    h.advance(Duration::from_millis(500)).await;
    assert_eq!(*origins.lock().unwrap(), ["external"], "the re-consult is still delayed");
    assert_eq!(replacing(&limiter), 1, "the refused failover route's admit");
    let dead = rig.restart().await;

    let mut carol_uas = carol.receive("INVITE").await;
    assert_eq!(
        *origins.lock().unwrap(),
        ["external", "call_limiter", "call_limiter"],
        "x, dropped by the refusal, was checked as added and refused"
    );
    assert_eq!(replacing(&limiter), 2, "the re-consult's [x] admit, refused");
    assert_eq!(rig.store.stats().admit_reregistered_calls, 0);
    rig.expect_holds([0, 0, 0], "nothing is held for the call").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("nothing is held for the call").await;
    // The SUT's ledger reads the dead store: the witnesses alone.
    assert_eq!(dead.stats().current_total, 3);
    b2bua.assert_fully_reaped_leaving(LimiterLeak {
        unreleased: 0,
        stored: dead.stats().current_total,
        queued: Vec::new(),
    });
    let _ = h.finish().await;
}

/// The fold that replaces the set also ends the call. Initial `[x, x]`, the
/// failover route `[x]` points at a host the destination allow-list refuses:
/// the fold's dispatching task replaced the set with `[x]`, the fold's turn
/// states it and terminates the call, whose settle releases the call once.
/// The store drains to the witnesses.
#[tokio::test(start_paused = true)]
async fn failover_fold_that_ends_the_call_releases_the_call_once() {
    let h = Harness::new("reroute-holds-fold-ends-the-call");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, &["x", "x"])))
            .on_failure(|_| CallTreatment::Route(limited_route("unlisted-host", 5071, &["x"])))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([2, 0, 0], "the initial route holds x twice").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the fold's CreateLeg is refused: the call ends in the fold's turn ──
    call.expect(503).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the one release frees the set the fold left").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released), (3, 3), "two holds replaced, one released");
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![503],
        "the a-leg INVITE transaction carries exactly one final: the 503",
    );
}

/// Release reroute of an established call: the route `[x, y]` is replaced by
/// the reroute `[y, z]` toward a media server. Once the reroute is applied the
/// call holds `y` and `z` once each; the hangup releases them.
#[tokio::test(start_paused = true)]
async fn release_reroute_replaces_the_route_holds() {
    let h = Harness::new("reroute-holds-release-reroute");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(
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
    );
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

    // ── the cap raises the release consult; the reroute is applied ─────────
    h.advance(Duration::from_secs(61)).await;
    let mut media_uas = media.receive("INVITE").await;
    rig.expect_holds([0, 1, 1], "the reroute's holds alone once it is applied").await;
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
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// An established call holding `[x]` whose duration cap raises a release
/// reroute stating `reroute`; the reroute's admit lands on the limiter and its
/// answer is lost. The call stays counted on `[x]` and the reroute is applied;
/// the rerouted call then outlives two leases (refreshing every 5 s) and
/// `held` is what the limiter holds for it throughout. No set lapses and the
/// hangup's one release drains the call. Returns, read before the hangup, the
/// store's re-registrations and the SUT's refreshes answered `Dropped`.
async fn release_reroute_admit_answer_lost(
    name: &str,
    reroute: &'static [&'static str],
    held: [i64; 3],
) -> (u64, u64) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig_with(LimiterConfig { lease_sec: SHORT_LEASE_SEC }).await;
    let limiter = unavailable_on_nth(2, true, rig.client.clone());
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_sec = 60;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&["x"]);
                NewCallResponse::Route(r)
            })
            .on_release(move |_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(reroute);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter)
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
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── the cap raises the release consult; the reroute is applied ─────────
    for _ in 0..61 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut retrans) = media.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    rig.expect_holds(held, "the set the limiter holds for the rerouted call").await;

    // ── the rerouted call outlives two leases ───────────────────────────────
    for _ in 0..2 * SHORT_LEASE_SEC + 5 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.all_holds(), held, "the limiter holds the same set two leases on");
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "no set lapsed");
    let reregistered = rig.store.stats().reregistered_calls;
    let refresh_dropped = b2bua.metrics().limiter().refresh_answers_total(RefreshOutcome::Dropped);

    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    settle_until(|| rig.store.stats().releases_total == 1).await;
    assert_eq!(rig.store.stats().releases_total, 1, "one release of the call");
    rig.expect_drained("the hangup's release frees what the limiter holds").await;
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "freed by its release, not its lease");
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (1, 1, 1));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
    (reregistered, refresh_dropped)
}

/// A release reroute `[y, z]` whose admit lands with its answer lost: the
/// call stays counted, and its refresh (naming `[x]`) extends the `[y, z]` the
/// limiter holds for it until its release.
#[tokio::test(start_paused = true)]
async fn release_reroute_admit_that_lands_but_times_out_keeps_the_call_counted() {
    let (reregistered, refresh_dropped) = release_reroute_admit_answer_lost(
        "reroute-holds-release-reroute-answer-lost",
        &["y", "z"],
        [0, 1, 1],
    )
    .await;
    assert_eq!(reregistered, 0, "the landed set was extended, never re-created");
    assert_eq!(refresh_dropped, 0);
}

/// A release reroute stating no limiter whose admit lands with its answer
/// lost: the limiter dropped the call's `[x]` behind a drop fence. The call's
/// next refresh learns the set was dropped and the call goes uncounted, so `x`
/// is never re-registered, not even once the fence lapses; the hangup still
/// sends the call's one release.
#[tokio::test(start_paused = true)]
async fn release_reroute_admit_that_dropped_the_set_is_never_re_registered() {
    let (reregistered, refresh_dropped) = release_reroute_admit_answer_lost(
        "reroute-holds-release-reroute-dropped-answer-lost",
        &[],
        [0, 0, 0],
    )
    .await;
    assert_eq!(reregistered, 0, "the dropped set was never re-created");
    assert_eq!(refresh_dropped, 1, "one refresh learnt the drop; the call refreshed no more");
}

/// Counts `call_release` consults and answers each one `delay` late.
struct SlowRelease {
    delay: Duration,
    releases: Arc<AtomicUsize>,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for SlowRelease {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
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
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.inner.call_release(req).await
    }
}

/// A release consult in flight is the call's only one: the cap that raised it
/// is spent, and nothing the established call does while the consult is
/// pending raises another. The consult of `[x, y]`'s call answers 150 s late
/// with the reroute `[y, z]`; meanwhile the cap's period elapses twice more,
/// the call refreshes its set and the caller re-INVITEs. One consult, one
/// replacing admit: the route the call applies is the set the limiter holds,
/// and the hangup drains it.
#[tokio::test(start_paused = true)]
async fn release_consult_in_flight_is_the_only_one_of_the_call() {
    const CAP_SEC: i64 = 60;
    const CONSULT_SEC: u64 = 150;
    let h = Harness::new("reroute-holds-release-consult-in-flight");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let limiter = spy(rig.client.clone());
    let releases = Arc::new(AtomicUsize::new(0));
    let decision = Arc::new(SlowRelease {
        delay: Duration::from_secs(CONSULT_SEC),
        releases: releases.clone(),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.features.platform.max_duration_sec = CAP_SEC;
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
        .limiter(limiter.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.call_control_timeout_ms = 1_000 * (CONSULT_SEC as i64 + 30);
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

    // ── the cap raises the release consult, which stays pending ─────────────
    for _ in 0..CAP_SEC + 1 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    settle_until(|| releases.load(Ordering::SeqCst) == 1).await;

    // The caller refreshes the session while the consult is pending.
    let mut reinv = dialog.reinvite(Some(ALICE_REALIGN)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // Two more cap periods pass and the call refreshes its set.
    for _ in 0..2 * CAP_SEC + 10 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult while it is pending");
    assert_eq!(replacing(&limiter), 0, "no replacing admit before the consult answers");
    rig.expect_holds([1, 1, 0], "the call still holds x and y").await;

    // ── the consult answers; the reroute is applied ─────────────────────────
    for _ in 0..30 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut retrans) = media.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    rig.expect_holds([0, 1, 1], "the applied reroute is the set the limiter holds").await;
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult for the call");
    assert_eq!(replacing(&limiter), 1, "one replacing admit for the call");

    // ── the rerouted call ends normally ─────────────────────────────────────
    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (4, 4, 0));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}
