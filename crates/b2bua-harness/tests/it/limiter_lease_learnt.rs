//! The worker's limiter lease is the one the limiter states.
//!
//! Every admit and refresh answer carries the limiter's lease. The worker
//! gives up a queued release, and a refresh due, one lease as it last learnt
//! it after the entry was queued or first marked, whatever lease the worker
//! would assume before any answer; and it warns and counts, once per change,
//! a learnt lease that its refresh period plus one refresh tick reaches.
//!
//! Every call holds three limiters, each id with one witness hold.

use call::LimiterEntry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::config::B2buaConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::metrics::{RefreshGiveUp, ReleaseGiveUp};
use b2bua_harness::{B2buaSut, WitnessRig};
use call::TimerType;
use call_limiter::LimiterConfig;
use http_net::{HttpRequest, HttpResponse, HttpService};
use scenario_harness::{Agent, Dialog, Harness, SIMULATED_TRANSIT_DELAY_MS};
use tokio::sync::watch;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// The refresh period the SUT runs.
const REFRESH_SEC: i64 = 5;
/// The lease a worker assumes before any limiter answer.
const WORKER_DEFAULT_LEASE_SEC: u64 = 120;
/// The three limiters every call holds.
const HOLDS: &[(&str, i64)] = &[("x", 10), ("y", 10), ("z", 10)];

/// Which requests the limiter swallows: they arrive and are neither applied
/// nor answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stall {
    Nothing,
    Releases,
    Refreshes,
}

/// The limiter server behind a switch that swallows some requests.
struct Stalling {
    inner: Arc<dyn HttpService>,
    stall: watch::Receiver<Stall>,
    paths: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl HttpService for Stalling {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.paths.lock().unwrap().push(req.path.clone());
        let swallowed = match *self.stall.borrow() {
            Stall::Nothing => false,
            Stall::Releases => req.path == "/v1/release",
            Stall::Refreshes => req.path == "/v1/refresh",
        };
        if swallowed {
            std::future::pending::<()>().await;
        }
        self.inner.handle(req).await
    }
}

/// A scenario: the witness rig under the limiter's `lease_sec`, behind
/// [`Stalling`], and the SUT on it with its own lease setting left alone.
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    rig: WitnessRig,
    stall: watch::Sender<Stall>,
    paths: Arc<Mutex<Vec<String>>>,
    b2bua: B2buaSut,
}

impl Scene {
    async fn new(
        name: &str,
        lease_sec: i64,
        tune: impl FnOnce(&mut B2buaConfig) + 'static,
    ) -> Self {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (stall, rx) = watch::channel(Stall::Nothing);
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seam = paths.clone();
        let rig =
            WitnessRig::serve_wrapped(LimiterConfig { lease_sec }, ADMIT_BUDGET, move |inner| {
                Arc::new(Stalling { inner, stall: rx.clone(), paths: seam.clone() })
            })
            .await;
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.call_limiter = HOLDS
                        .iter()
                        .map(|(id, limit)| LimiterEntry { id: (*id).into(), limit: *limit })
                        .collect();
                    NewCallResponse::Route(r)
                })
                .build(),
        );
        let b2bua = B2buaSut::builder(decision)
            .limiter(rig.client.clone())
            .limiter_store(rig.store.clone())
            .tune(move |c| {
                c.keepalive_interval_sec = 3_600;
                c.limiter_refresh_sec = REFRESH_SEC;
                tune(c);
            })
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        Self { h, alice, bob, rig, stall, paths, b2bua }
    }

    /// INVITE → 200 → ACK, relayed.
    async fn establish(&self) -> Dialog {
        let mut call =
            self.alice.invite(&self.bob).with_sdp(OFFER).through(self.b2bua.addr).send().await;
        let mut uas = self.bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        self.bob.receive("ACK").await;
        dialog
    }

    /// The caller's BYE, answered by the callee and relayed back.
    async fn hang_up(&self, dialog: &mut Dialog) {
        let mut bye = dialog.bye().await;
        self.bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        self.h.advance(Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS)).await;
    }

    /// Advance `secs` seconds one at a time, keeping the witnesses alive.
    async fn hold_for(&self, secs: u64) {
        for _ in 0..secs {
            self.h.advance(Duration::from_secs(1)).await;
            self.rig.refresh_witnesses();
        }
    }

    fn queued(&self) -> u64 {
        self.b2bua.metrics().limiter().release_queue_depth()
    }

    /// How many requests on `path` the limiter received.
    fn received(&self, path: &str) -> usize {
        self.paths.lock().unwrap().iter().filter(|p| *p == path).count()
    }

    /// The value of the metric line starting with `series` in the SUT's
    /// exposition, if exposed.
    fn exposed(&self, series: &str) -> Option<String> {
        let text = self.b2bua.metrics().prometheus_text();
        text.lines()
            .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ').map(str::to_string))
    }
}

/// A limiter whose lease (20 s) is shorter than the worker's default: a
/// release queued while it stalls is given up 20 s after it was queued, not
/// at the worker's default.
#[tokio::test(start_paused = true)]
async fn a_queued_release_is_given_up_one_limiter_lease_after_it_was_queued() {
    const LEASE_SEC: i64 = 20;
    let s = Scene::new("lease-learnt-release-shorter", LEASE_SEC, |_| {}).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    s.stall.send_replace(Stall::Releases);
    s.hang_up(&mut dialog).await;
    sip_clock::testkit::settle().await;
    assert!(s.b2bua.calls_reaped());
    assert_eq!(s.queued(), 1, "the release waits in the queue");

    s.hold_for(LEASE_SEC as u64 - 2).await;
    let metrics = s.b2bua.metrics();
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired),
        0,
        "inside the lease"
    );
    assert_eq!(s.queued(), 1);

    s.hold_for(4).await;
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired),
        1,
        "given up one limiter lease ({LEASE_SEC} s) after it was queued, not after the \
         worker's default {WORKER_DEFAULT_LEASE_SEC} s"
    );
    assert_eq!(s.queued(), 0);

    s.stall.send_replace(Stall::Nothing);
    s.hold_for(5).await;
    s.rig.expect_drained("the limiter's lease freed the call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter whose lease (200 s) is longer than the worker's default: a
/// release queued while it stalls is still sent past the worker's default,
/// and frees the call the limiter still counts.
#[tokio::test(start_paused = true)]
async fn a_queued_release_is_kept_as_long_as_the_limiters_longer_lease() {
    const LEASE_SEC: i64 = 200;
    let s = Scene::new("lease-learnt-release-longer", LEASE_SEC, |_| {}).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    s.stall.send_replace(Stall::Releases);
    s.hang_up(&mut dialog).await;
    sip_clock::testkit::settle().await;
    assert!(s.b2bua.calls_reaped());

    s.hold_for(WORKER_DEFAULT_LEASE_SEC + 10).await;
    let metrics = s.b2bua.metrics();
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired),
        0,
        "the limiter still counts the call: its release is kept"
    );
    assert_eq!(s.queued(), 1, "the release still waits");
    assert_eq!(s.rig.all_holds(), [1, 1, 1], "the limiter still counts the call");

    s.stall.send_replace(Stall::Nothing);
    s.hold_for(10).await;
    s.rig.expect_drained("the queued release freed the call").await;
    assert_eq!(s.rig.store.stats().lease_expired_calls, 0, "the release freed it, not the lease");
    assert_eq!(s.queued(), 0);
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter whose lease (20 s) is shorter than the worker's default: a
/// refresh due while the limiter swallows refreshes is given up 20 s after
/// its first mark, and the call's next refresh marks it again.
#[tokio::test(start_paused = true)]
async fn a_refresh_due_is_given_up_one_limiter_lease_after_its_first_mark() {
    const LEASE_SEC: i64 = 20;
    let s = Scene::new("lease-learnt-refresh-shorter", LEASE_SEC, |_| {}).await;
    s.stall.send_replace(Stall::Refreshes);
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    // First mark at one refresh period; given up one lease later.
    s.hold_for(REFRESH_SEC as u64 + LEASE_SEC as u64 + 6).await;
    let metrics = s.b2bua.metrics();
    assert!(s.received("/v1/refresh") > 0, "the batch kept trying");
    assert_eq!(
        metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired),
        1,
        "given up one limiter lease ({LEASE_SEC} s) after its first mark, not after the \
         worker's default {WORKER_DEFAULT_LEASE_SEC} s"
    );

    // The limiter answers again: the call's next refresh re-registers the
    // set its lease dropped.
    s.stall.send_replace(Stall::Nothing);
    s.hold_for(REFRESH_SEC as u64 + 2).await;
    s.rig.expect_holds([1, 1, 1], "the refresh re-registered the call").await;

    s.hang_up(&mut dialog).await;
    s.hold_for(2).await;
    s.rig.expect_drained("the release freed the call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter whose lease (4 s) its workers' refresh period (5 s) plus a tick
/// (1 s) reaches: the worker warns and counts it once, at the first answer
/// that states it, and the calls still end drained.
#[tokio::test(start_paused = true)]
async fn a_limiter_lease_the_refresh_reaches_is_counted_once_per_change() {
    const LEASE_SEC: i64 = 4;
    let s = Scene::new("lease-learnt-too-short", LEASE_SEC, |_| {}).await;
    let too_short = "b2bua_limiter_lease_too_short_total";

    let mut first = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the first call holds its three limiters").await;
    assert_eq!(s.exposed(too_short).as_deref(), Some("1"), "counted at the first answer");
    assert_eq!(s.exposed("b2bua_limiter_lease_seconds").as_deref(), Some("4"));

    let mut second = s.establish().await;
    s.rig.expect_holds([2, 2, 2], "both calls hold their three limiters").await;
    s.hold_for(REFRESH_SEC as u64 + 2).await;
    assert_eq!(
        s.exposed(too_short).as_deref(),
        Some("1"),
        "the same lease stated again counts nothing"
    );

    s.hang_up(&mut first).await;
    s.hang_up(&mut second).await;
    s.hold_for(2).await;
    s.rig.expect_drained("the releases freed both calls").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter at the default lease (120 s) that the worker's refresh period
/// (120 s) plus a tick reaches: the first lease stated is checked even when
/// it equals the lease the worker assumed before any answer.
#[tokio::test(start_paused = true)]
async fn a_first_stated_lease_equal_to_the_assumed_one_is_checked() {
    let s = Scene::new("lease-learnt-first-default", 120, |c| c.limiter_refresh_sec = 120).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;
    assert_eq!(
        s.exposed("b2bua_limiter_lease_too_short_total").as_deref(),
        Some("1"),
        "the first stated lease is checked"
    );
    s.hang_up(&mut dialog).await;
    s.hold_for(2).await;
    s.rig.expect_drained("the release freed the call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter whose lease (6 s) is below three of the worker's refresh
/// periods (40 s): the worker refreshes every third of the lease instead, so
/// the call's set never lapses, and counts the clamp.
#[tokio::test(start_paused = true)]
async fn a_short_lease_shortens_the_refresh_period_to_a_third_of_it() {
    const LEASE_SEC: i64 = 6;
    let s =
        Scene::new("lease-learnt-refresh-clamped", LEASE_SEC, |c| c.limiter_refresh_sec = 40).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    for second in 0..30 {
        s.hold_for(1).await;
        assert_eq!(s.rig.all_holds(), [1, 1, 1], "the call's set never lapses ({second} s)");
    }
    assert_eq!(s.rig.store.stats().lease_expired_calls, 0, "no set lapsed");
    assert_eq!(
        s.exposed("b2bua_limiter_refresh_period_clamped_total").as_deref(),
        Some("1"),
        "the clamp is counted once"
    );
    assert_eq!(s.exposed("b2bua_limiter_refresh_period_seconds").as_deref(), Some("2"));

    s.hang_up(&mut dialog).await;
    s.hold_for(2).await;
    s.rig.expect_drained("the release freed the call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A limiter whose lease (6 s) sets the refresh period to a third of it
/// (2 s), below the configured one (40 s): a call admitted once the worker
/// learnt the lease has its first refresh due one learnt period after its
/// admit, and the refresh leaves then.
#[tokio::test(start_paused = true)]
async fn a_call_s_first_refresh_falls_due_one_learnt_period_after_its_admit() {
    const LEASE_SEC: i64 = 6;
    const LEARNT_PERIOD_MS: i64 = 2_000;
    const CALL_ID: &str = "first-refresh@127.0.0.1";
    const FROM_TAG: &str = "first-refresh-tag";
    let s =
        Scene::new("lease-learnt-first-refresh", LEASE_SEC, |c| c.limiter_refresh_sec = 40).await;

    // The first call's admit answer teaches the worker the lease.
    let mut first = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the first call holds its three limiters").await;
    assert_eq!(s.exposed("b2bua_limiter_refresh_period_seconds").as_deref(), Some("2"));
    s.hang_up(&mut first).await;
    s.hold_for(2).await;
    s.rig.expect_holds([0, 0, 0], "the release freed the first call").await;
    let refreshes = s.received("/v1/refresh");

    // The admit runs on the turn of the INVITE, one transit after it leaves.
    let admitted_at = s.b2bua.clock().now_ms() + SIMULATED_TRANSIT_DELAY_MS as i64;
    let mut call = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.rig.expect_holds([1, 1, 1], "the second call holds its three limiters").await;

    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let live = s.b2bua.live_call(&call_ref).expect("the call is live");
    let refresh = live
        .timers
        .iter()
        .find(|t| t.timer_type == TimerType::LimiterRefresh)
        .expect("the counted call has its refresh armed");
    assert_eq!(
        refresh.fire_at - admitted_at,
        LEARNT_PERIOD_MS,
        "the first refresh is due one learnt period after the admit"
    );

    s.hold_for(1).await;
    assert_eq!(s.received("/v1/refresh"), refreshes, "not due before one learnt period");
    s.hold_for(2).await;
    assert_eq!(
        s.received("/v1/refresh"),
        refreshes + 1,
        "the first refresh left one learnt period after the admit"
    );

    s.hang_up(&mut dialog).await;
    s.hold_for(2).await;
    s.rig.expect_drained("the release freed the second call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}
