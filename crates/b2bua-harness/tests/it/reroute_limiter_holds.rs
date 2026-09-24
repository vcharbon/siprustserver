//! A reroute's limiter holds replace the replaced route's.
//!
//! The limiter holds of a call belong to its latest applied route. When a
//! failover route or a release reroute is applied, the holds the call carries
//! from the route it replaces are released, and the new route's admitted holds
//! (if any) become the call's. A route that states no limiter, or whose admit
//! fails open, still replaces: the call is then uncounted.
//!
//! Every scenario carries several limiters: distinct ids on one route, the
//! same id on the replaced and the new route, two holds on one
//! `(id, window)`, overlapping sets. Each id carries one **witness** hold
//! admitted outside the call, so a surplus release shows as a count below the
//! witness instead of vanishing under the store's floor at 0. The store is
//! probed per id while the call is up and drained to the witnesses after it
//! ends.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallLimiterEntry, CallReleaseResponse, CallTreatment, NewCallResponse, ReleaseOutcome,
    ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold};
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use call::ReleaseEventKind;
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Every id a scenario may hold; each carries one witness hold.
const IDS: [&str; 3] = ["x", "y", "z"];

/// A real `LimiterServer` on the simulated HTTP fabric, so every admit is a
/// genuine hold the store counts, with one witness hold per id in [`IDS`].
struct LimiterRig {
    store: Arc<WindowStore>,
    client: Arc<dyn CallLimiter>,
    _server: Box<dyn HttpServerHandle>,
}

impl LimiterRig {
    /// The holds the call owns on `id`: the store's count less the witness.
    /// Negative = a release matched no hold of the call.
    fn holds(&self, id: &str) -> i64 {
        self.store.held(id) - 1
    }

    /// The call's holds on every id of [`IDS`], in order.
    fn all_holds(&self) -> [i64; 3] {
        IDS.map(|id| self.holds(id))
    }

    /// Settle until the call's holds read `expected`, then assert them.
    async fn expect_holds(&self, expected: [i64; 3], why: &str) {
        settle_until(|| self.all_holds() == expected).await;
        assert_eq!(self.all_holds(), expected, "holds on {IDS:?}: {why}");
    }
}

async fn limiter_rig() -> LimiterRig {
    let laddr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    for id in IDS {
        let witness = store.admit(&[AdmitEntry { id: id.into(), limit: 100 }]);
        assert!(matches!(witness, AdmitResult::Admitted { .. }), "witness hold on {id}");
    }
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = http.serve(laddr, server).await.unwrap();
    // A fail-open budget above the paused-clock HTTP round trip: the detached
    // admit is woken inside a coarse `h.advance`, whose 100 ms chunks a
    // production-sized budget could expire between.
    let client: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, Duration::from_secs(2)));
    LimiterRig { store, client, _server: handle }
}

/// A limiter whose `n`-th admit (1-based) is unavailable — the fail-open
/// outcome of a stalled or unreachable limiter — and which otherwise delegates.
struct UnavailableOnAdmit {
    n: usize,
    admits: AtomicUsize,
    inner: Arc<dyn CallLimiter>,
}

#[async_trait]
impl CallLimiter for UnavailableOnAdmit {
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome {
        if self.admits.fetch_add(1, Ordering::SeqCst) + 1 == self.n {
            return AdmitOutcome::Unavailable;
        }
        self.inner.admit(entries).await
    }
    async fn release(&self, holds: &[LimiterHold]) {
        self.inner.release(holds).await
    }
    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        self.inner.refresh(holds).await
    }
}

fn limiters(ids: &[&str]) -> Vec<CallLimiterEntry> {
    ids.iter().map(|id| CallLimiterEntry { id: (*id).into(), limit: 10 }).collect()
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
/// call's expected holds on [`IDS`] while bob is dialed and once the failover
/// route is applied.
async fn busy_then_failover_answered(
    initial: &'static [&'static str],
    failover: &'static [&'static str],
    rig: LimiterRig,
    limiter: Arc<dyn CallLimiter>,
    name: &str,
    before: [i64; 3],
    after: [i64; 3],
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(one_failover(initial, failover))
        .limiter(limiter)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds(before, "the initial route's holds while bob is dialed").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route is applied: its holds replace the initial ones ──
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
    b2bua.assert_fully_reaped();
    rig.expect_holds([0, 0, 0], "the hangup releases the failover route's holds").await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// Initial `[x, y]`, failover `[y, z]` (an overlapping set): once the failover
/// is applied the call holds `y` and `z` once each and nothing on `x`.
#[tokio::test(start_paused = true)]
async fn failover_route_replaces_the_initial_route_holds() {
    let rig = limiter_rig().await;
    let limiter = rig.client.clone();
    busy_then_failover_answered(
        &["x", "y"],
        &["y", "z"],
        rig,
        limiter,
        "reroute-holds-failover-overlapping",
        [1, 1, 0],
        [0, 1, 1],
    )
    .await;
}

/// Initial `[x, x]` (two holds on one `(id, window)`), failover `[x]`: both
/// replaced holds are released, the failover's one remains.
#[tokio::test(start_paused = true)]
async fn failover_route_on_the_same_id_holds_it_once() {
    let rig = limiter_rig().await;
    let limiter = rig.client.clone();
    busy_then_failover_answered(
        &["x", "x"],
        &["x"],
        rig,
        limiter,
        "reroute-holds-failover-same-id",
        [2, 0, 0],
        [1, 0, 0],
    )
    .await;
}

/// A failover route stating no limiter replaces `[x, y]` with nothing.
#[tokio::test(start_paused = true)]
async fn failover_route_without_limiters_releases_the_initial_route_holds() {
    let rig = limiter_rig().await;
    let limiter = rig.client.clone();
    busy_then_failover_answered(
        &["x", "y"],
        &[],
        rig,
        limiter,
        "reroute-holds-failover-unlimited",
        [1, 1, 0],
        [0, 0, 0],
    )
    .await;
}

/// The failover route `[y, z]`'s admit fails open (the limiter is unavailable
/// for that admit only): the call proceeds uncounted, and `[x]`, the replaced
/// route's hold, is released all the same.
#[tokio::test(start_paused = true)]
async fn failover_route_admitted_fail_open_releases_the_initial_route_holds() {
    let rig = limiter_rig().await;
    let limiter: Arc<dyn CallLimiter> = Arc::new(UnavailableOnAdmit {
        n: 2,
        admits: AtomicUsize::new(0),
        inner: rig.client.clone(),
    });
    busy_then_failover_answered(
        &["x"],
        &["y", "z"],
        rig,
        limiter,
        "reroute-holds-failover-fail-open",
        [1, 0, 0],
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
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    rig.expect_holds([0, 0, 0], "the hangup releases the latest route's hold").await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// The fold that replaces the holds also ends the call. Initial `[x, x]`, the
/// failover route `[x]` points at a host the target admission gate refuses:
/// the fold's turn records the new hold, releases the two replaced ones and
/// terminates the call, whose settle releases the new one. Three releases for
/// three holds on one `(x, window)`: the replaced holds' releases never stand
/// in for the new hold's.
#[tokio::test(start_paused = true)]
async fn failover_fold_that_ends_the_call_releases_every_hold_once() {
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
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([2, 0, 0], "the initial route holds x twice").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the fold's CreateLeg is refused: the call ends in the fold's turn ──
    call.expect(503).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    rig.expect_holds([0, 0, 0], "every hold on (x, window) is released exactly once").await;

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
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    rig.expect_holds([0, 0, 0], "the hangup releases the reroute's holds").await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}
