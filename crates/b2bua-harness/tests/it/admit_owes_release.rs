//! A call that sent an admit request owes one release of its key.
//!
//! Whatever the admit's answer (admitted, refused at a cap, refused by a
//! release fence, a timeout, a transport error, a non-200), the request may
//! have reached the limiter, so the call releases its key once at its end.
//! The release frees exactly what the limiter holds for the call and nothing
//! otherwise; it fences the key, so an admit that lands after it holds
//! nothing. Refresh stays reserved to a call the limiter confirmed: an
//! initial admit that failed never refreshes. An admit that sent no request
//! owes nothing by itself.
//!
//! Every scenario carries several limiters, each id with one **witness**
//! hold admitted under a call of its own, so a surplus release reads below
//! the witness instead of vanishing under the store's floor at 0.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, NoopLimiter, RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut, WitnessRig};
use call_limiter::LimiterConfig;
use http_net::{HttpRequest, HttpResponse, HttpService};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// A short lease, so the paused clock crosses it cheaply.
const LEASE_SEC: i64 = 20;
/// The refresh period the SUT runs: several fire while a call is up.
const REFRESH_SEC: i64 = 5;

/// What the limiter does with an admit request.
#[derive(Clone, Copy)]
enum AdmitFault {
    /// Never applied; answered past the client's budget.
    NeverLands,
    /// Applied at once; answered past the client's budget.
    AnswersLate,
    /// Applied `after` it arrived, detached from the request; answered past
    /// the client's budget.
    LandsAfter(Duration),
    /// Applied and answered at once.
    None,
}

/// The limiter server behind a log of every request path it received, in
/// arrival order, and a fault on `/v1/admit`.
struct FaultyAdmits {
    inner: Arc<dyn HttpService>,
    fault: AdmitFault,
    paths: Arc<Mutex<Vec<String>>>,
}

/// How long past the admit budget a faulted admit is answered.
const LATE: Duration = Duration::from_millis(400);

#[async_trait]
impl HttpService for FaultyAdmits {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.paths.lock().unwrap().push(req.path.clone());
        if req.path != "/v1/admit" {
            return self.inner.handle(req).await;
        }
        match self.fault {
            AdmitFault::None => self.inner.handle(req).await,
            AdmitFault::NeverLands => {
                tokio::time::sleep(LATE).await;
                HttpResponse::status(504)
            }
            AdmitFault::AnswersLate => {
                let resp = self.inner.handle(req).await;
                tokio::time::sleep(LATE).await;
                resp
            }
            AdmitFault::LandsAfter(after) => {
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = inner.handle(req).await;
                });
                tokio::time::sleep(LATE).await;
                HttpResponse::status(504)
            }
        }
    }
}

/// The witness rig under a short lease and the production admit budget,
/// its server behind [`FaultyAdmits`]; returns the request log.
async fn rig_with(fault: AdmitFault) -> (WitnessRig, Arc<Mutex<Vec<String>>>) {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let log = paths.clone();
    let rig =
        WitnessRig::serve_wrapped(LimiterConfig { lease_sec: LEASE_SEC }, ADMIT_BUDGET, move |s| {
            Arc::new(FaultyAdmits { inner: s, fault, paths: log.clone() })
        })
        .await;
    (rig, paths)
}

fn limiters(entries: &[(&str, i64)]) -> Vec<CallLimiterEntry> {
    entries.iter().map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit }).collect()
}

/// A route toward bob (5070) holding `entries`.
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

/// The SUT on `limiter` (reading `rig`'s store), refreshing every
/// [`REFRESH_SEC`], with no keepalive inside a scenario.
async fn sut(
    h: &Harness,
    decision: Arc<ScriptedDecisionEngine>,
    limiter: Arc<dyn CallLimiter>,
    rig: &WitnessRig,
) -> B2buaSut {
    B2buaSut::builder(decision)
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.limiter_refresh_sec = REFRESH_SEC;
        })
        .start(h, "b2bua", "127.0.0.1:5080")
        .await
}

/// Advance `secs` seconds one at a time, keeping the witnesses' leases alive.
async fn hold_for(h: &Harness, rig: &WitnessRig, secs: u64) {
    for _ in 0..secs {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
}

/// The request paths the limiter received after the first admit.
fn after_first_admit(paths: &Mutex<Vec<String>>) -> Vec<String> {
    let paths = paths.lock().unwrap();
    let first = paths.iter().position(|p| p == "/v1/admit").expect("an admit was received");
    paths[first + 1..].to_vec()
}

/// The initial admit times out and never lands: the call runs uncounted,
/// sends no refresh over several refresh periods, and sends one release at
/// its end. The limiter holds nothing for it at any point; the release
/// creates no set and fences the key.
#[tokio::test(start_paused = true)]
async fn an_initial_admit_that_never_lands_owes_one_release_and_no_refresh() {
    let h = Harness::new("admit-owes-release-never-lands");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = rig_with(AdmitFault::NeverLands).await;
    let b2bua = sut(&h, routes_holding(&[("x", 10), ("y", 10)]), rig.client.clone(), &rig).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hold_for(&h, &rig, 3 * REFRESH_SEC as u64).await;
    assert_eq!(rig.all_holds(), [0, 0, 0], "the admit never landed");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    settle_until(|| rig.store.stats().releases_total == 1).await;

    assert_eq!(
        after_first_admit(&paths),
        ["/v1/release"],
        "no refresh of an unconfirmed call, one release at its end",
    );
    let stats = rig.store.stats();
    assert_eq!(stats.releases_total, 1, "one release of the call");
    assert_eq!(stats.calls, 3, "the release created no set: the witnesses alone");
    assert_eq!(stats.fences, 1, "the release fenced the call's key");
    assert_eq!(stats.lease_expired_calls, 0);
    rig.expect_drained("the call never held anything").await;
    assert_eq!(b2bua.limiter_count().failed_open, 1);
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The initial admit lands on the limiter but its answer comes past the
/// client's budget: the call runs uncounted and sends no refresh, and its
/// terminal release frees the set the late admit created, inside the lease.
#[tokio::test(start_paused = true)]
async fn an_initial_admit_that_times_out_and_lands_late_is_freed_by_the_terminal_release() {
    let h = Harness::new("admit-owes-release-lands-late");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = rig_with(AdmitFault::AnswersLate).await;
    let b2bua = sut(&h, routes_holding(&[("x", 10), ("y", 10)]), rig.client.clone(), &rig).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the late admit counted the call on the limiter").await;
    hold_for(&h, &rig, 2 * REFRESH_SEC as u64 + 1).await;
    rig.expect_holds([1, 1, 0], "the set lives on inside its lease").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_holds([0, 0, 0], "the terminal release freed the late set").await;

    assert_eq!(
        after_first_admit(&paths),
        ["/v1/release"],
        "no refresh of an unconfirmed call, one release at its end",
    );
    let stats = rig.store.stats();
    assert_eq!(stats.lease_expired_calls, 0, "the release freed it, not the lease");
    assert_eq!(stats.releases_total, 1);
    rig.expect_drained("released").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.failed_open, count.admitted, count.released), (1, 0, 0));
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The initial admit times out and lands on the limiter only after the call
/// ended: the call's release fenced the key first, so the late admit is
/// refused by the fence and creates nothing.
#[tokio::test(start_paused = true)]
async fn an_initial_admit_landing_after_the_release_is_refused_by_the_fence() {
    let h = Harness::new("admit-owes-release-lands-after-release");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = rig_with(AdmitFault::LandsAfter(Duration::from_secs(8))).await;
    let b2bua = sut(&h, routes_holding(&[("x", 10), ("y", 10)]), rig.client.clone(), &rig).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(rig.all_holds(), [0, 0, 0], "the admit has not landed yet");

    hold_for(&h, &rig, 10).await;
    let stats = rig.store.stats();
    assert_eq!(stats.admits_refused_released, 1, "the late admit met the release fence");
    assert_eq!(rig.all_holds(), [0, 0, 0], "the late admit created nothing");
    assert_eq!(stats.calls, 3, "the witnesses alone");
    assert_eq!(after_first_admit(&paths), ["/v1/release"]);
    rig.expect_drained("nothing held").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// An initial admit refused at a cap sent a request: the call it ends in
/// (the stack's 486) releases its key once, which frees nothing and holds
/// nothing. `y` at cap 1 is refused (its witness fills it) on the list's
/// second entry.
#[tokio::test(start_paused = true)]
async fn an_initial_admit_refused_at_its_cap_owes_one_release() {
    let h = Harness::new("admit-owes-release-refused-at-cap");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, paths) = rig_with(AdmitFault::None).await;
    let b2bua = sut(&h, routes_holding(&[("x", 10), ("y", 1)]), rig.client.clone(), &rig).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    call.expect(486).await;
    settle_until(|| b2bua.is_reaped()).await;
    settle_until(|| rig.store.stats().releases_total == 1).await;

    assert_eq!(after_first_admit(&paths), ["/v1/release"]);
    let stats = rig.store.stats();
    assert_eq!(stats.calls, 3, "the release created no set");
    assert_eq!(stats.fences, 1, "the release fenced the call's key");
    rig.expect_drained("the refusal counted nothing").await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice.addr()), vec![486]);
}

/// Counts the requests a limiter received.
#[derive(Default)]
struct Requests {
    admits: AtomicUsize,
    releases: AtomicUsize,
    refreshes: AtomicUsize,
}

/// A limiter that sends no request (none configured) behind a count of what
/// the SUT asked of it.
struct CountedNoop(Arc<Requests>);

#[async_trait]
impl CallLimiter for CountedNoop {
    async fn admit(&self, key: &str, entries: &[LimiterEntry], release: bool) -> AdmitOutcome {
        self.0.admits.fetch_add(1, Ordering::SeqCst);
        NoopLimiter.admit(key, entries, release).await
    }
    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.0.releases.fetch_add(keys.len(), Ordering::SeqCst);
        NoopLimiter.release(keys).await
    }
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.0.refreshes.fetch_add(1, Ordering::SeqCst);
        NoopLimiter.refresh(calls).await
    }
}

/// An admit that sent no request (no limiter is configured) owes nothing:
/// the call neither refreshes nor releases.
#[tokio::test(start_paused = true)]
async fn an_admit_that_sent_no_request_owes_no_release() {
    let h = Harness::new("admit-owes-release-no-request");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (rig, _paths) = rig_with(AdmitFault::None).await;
    let requests = Arc::new(Requests::default());
    let limiter = Arc::new(CountedNoop(requests.clone()));
    let b2bua = sut(&h, routes_holding(&[("x", 10), ("y", 10)]), limiter, &rig).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hold_for(&h, &rig, 2 * REFRESH_SEC as u64 + 1).await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;

    assert_eq!(requests.admits.load(Ordering::SeqCst), 1);
    assert_eq!(requests.refreshes.load(Ordering::SeqCst), 0, "no refresh");
    assert_eq!(requests.releases.load(Ordering::SeqCst), 0, "no release owed");
    rig.expect_drained("nothing reached the limiter").await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
