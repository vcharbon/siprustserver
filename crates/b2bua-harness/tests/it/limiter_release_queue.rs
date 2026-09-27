//! The terminal limiter release never holds the call.
//!
//! A call's end writes its CDR and removes the call in its last turn, whatever
//! the limiter does: the release of its key goes to the worker's release
//! queue. The queue sends at once, batches every due key into one request,
//! retries a failed send with backoff, drops an entry unsent once it has
//! waited one lease (the limiter already let the call's set lapse) and drops
//! its oldest entry at its cap. Both drops are counted.
//!
//! Every call holds three limiters, each id with one **witness** hold
//! admitted under a call of its own, so a surplus release reads below the
//! witness instead of vanishing under the store's floor at 0.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::config::B2buaConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut, WitnessRig};
use call_limiter::LimiterConfig;
use http_net::{HttpRequest, HttpResponse, HttpService};
use scenario_harness::{Agent, Dialog, Harness, SIMULATED_TRANSIT_DELAY_MS};
use tokio::sync::watch;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// A short lease, so the paused clock crosses it cheaply; the SUT is told
/// the same lease.
const LEASE_SEC: i64 = 20;
/// The refresh period the SUT runs.
const REFRESH_SEC: i64 = 5;
/// The three limiters every call holds.
const HOLDS: &[(&str, i64)] = &[("x", 10), ("y", 10), ("z", 10)];

/// Which requests the limiter swallows: they arrive and are logged, and are
/// neither applied nor answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stall {
    Nothing,
    Releases,
    Everything,
}

/// One request the limiter received: its path and its JSON body.
type Received = (String, serde_json::Value);

/// The limiter server behind a log of every request it received, in arrival
/// order, and a switch that swallows some of them.
struct Stalling {
    inner: Arc<dyn HttpService>,
    stall: watch::Receiver<Stall>,
    log: Arc<Mutex<Vec<Received>>>,
}

#[async_trait]
impl HttpService for Stalling {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let body = serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
        self.log.lock().unwrap().push((req.path.clone(), body));
        let stall = *self.stall.borrow();
        let swallowed = match stall {
            Stall::Nothing => false,
            Stall::Releases => req.path == "/v1/release",
            Stall::Everything => true,
        };
        if swallowed {
            std::future::pending::<()>().await;
        }
        self.inner.handle(req).await
    }
}

/// The keys one received release names.
fn release_keys(body: &serde_json::Value) -> Vec<String> {
    let as_string = |v: &serde_json::Value| v.as_str().expect("a key is a string").to_string();
    match (body.get("keys"), body.get("key")) {
        (Some(keys), _) => keys.as_array().expect("keys is a list").iter().map(as_string).collect(),
        (None, Some(key)) => vec![as_string(key)],
        (None, None) => panic!("a release names no key: {body}"),
    }
}

/// A scenario: the witness rig behind [`Stalling`], and the SUT on it.
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    rig: WitnessRig,
    stall: watch::Sender<Stall>,
    log: Arc<Mutex<Vec<Received>>>,
    b2bua: B2buaSut,
}

impl Scene {
    async fn new(name: &str, tune: impl FnOnce(&mut B2buaConfig) + 'static) -> Self {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let (stall, rx) = watch::channel(Stall::Nothing);
        let log = Arc::new(Mutex::new(Vec::new()));
        let seam = log.clone();
        let rig = WitnessRig::serve_wrapped(
            LimiterConfig { lease_sec: LEASE_SEC },
            ADMIT_BUDGET,
            move |inner| Arc::new(Stalling { inner, stall: rx.clone(), log: seam.clone() }),
        )
        .await;
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.call_limiter = HOLDS
                        .iter()
                        .map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit })
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
                c.limiter_lease_sec = LEASE_SEC;
                tune(c);
            })
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        Self { h, alice, bob, rig, stall, log, b2bua }
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

    /// The caller's BYE, answered by the callee and relayed back; returns
    /// once the callee's 200 has reached the SUT (the call's last turn),
    /// without waiting any longer.
    async fn hang_up(&self, dialog: &mut Dialog) {
        let mut bye = dialog.bye().await;
        self.bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        b2bua_harness::advance(SIMULATED_TRANSIT_DELAY_MS).await;
    }

    /// Advance `secs` seconds one at a time, keeping the witnesses alive.
    async fn hold_for(&self, secs: u64) {
        for _ in 0..secs {
            self.h.advance(Duration::from_secs(1)).await;
            self.rig.refresh_witnesses();
        }
    }

    /// How many requests the limiter has received so far.
    fn received(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// The keys of every release received from index `from` on, one list
    /// per request.
    fn releases_since(&self, from: usize) -> Vec<Vec<String>> {
        let log = self.log.lock().unwrap();
        log[from..]
            .iter()
            .filter(|(path, _)| path == "/v1/release")
            .map(|(_, body)| release_keys(body))
            .collect()
    }

    /// The limiter key of every admit received, in arrival order.
    fn admitted_keys(&self) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|(path, _)| path == "/v1/admit")
            .map(|(_, body)| body["key"].as_str().expect("an admit names its key").to_string())
            .collect()
    }

    fn queued(&self) -> u64 {
        self.b2bua.metrics().limiter_release_queue_depth()
    }
}

/// Stalled limiter: a call holding three limiters ends; its CDR is written
/// and it is removed in its last turn, and its release waits in the queue.
#[tokio::test(start_paused = true)]
async fn a_call_ending_on_a_stalled_limiter_is_removed_in_its_last_turn() {
    let s = Scene::new("release-queue-stalled-limiter", |_| {}).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    s.stall.send_replace(Stall::Releases);
    s.hang_up(&mut dialog).await;
    sip_clock::testkit::settle().await;

    assert_eq!(s.b2bua.cdr_records().len(), 1, "the CDR is written in the call's last turn");
    assert!(s.b2bua.calls_reaped(), "the call is removed in its last turn");
    assert_eq!(s.queued(), 1, "one release waits in the queue");
    assert_eq!(s.rig.all_holds(), [1, 1, 1], "the stalled limiter has applied nothing");

    // The limiter answers again: the queue drains.
    s.stall.send_replace(Stall::Nothing);
    s.hold_for(10).await;
    s.rig.expect_drained("the queued release freed the call").await;
    assert_eq!(s.queued(), 0);
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The limiter comes back: every queued key leaves in one request, and the
/// count drains to 0.
#[tokio::test(start_paused = true)]
async fn the_limiter_back_drains_the_queue_in_one_batched_request() {
    let s = Scene::new("release-queue-batched-drain", |_| {}).await;
    let mut dialogs = Vec::new();
    for _ in 0..3 {
        dialogs.push(s.establish().await);
    }
    s.rig.expect_holds([3, 3, 3], "three calls, three limiters each").await;

    s.stall.send_replace(Stall::Releases);
    for dialog in &mut dialogs {
        s.hang_up(dialog).await;
    }
    sip_clock::testkit::settle().await;
    assert!(s.b2bua.calls_reaped(), "every call is removed while the limiter stalls");
    assert_eq!(s.b2bua.cdr_records().len(), 3);
    assert_eq!(s.queued(), 3, "three releases wait");
    s.hold_for(5).await;
    assert_eq!(s.rig.all_holds(), [3, 3, 3], "nothing applied while stalled");

    let before = s.received();
    s.stall.send_replace(Stall::Nothing);
    s.hold_for(15).await;

    let sent = s.releases_since(before);
    assert_eq!(sent.len(), 1, "one release request once the limiter answers: {sent:?}");
    let mut keys = sent[0].clone();
    keys.sort();
    let mut expected = s.admitted_keys();
    expected.sort();
    assert_eq!(keys, expected, "the request names every queued call");
    assert_eq!(s.queued(), 0, "the queue drained");
    assert!(s.b2bua.metrics().limiter_release_retries_total() >= 3, "each key was retried");
    s.rig.expect_drained("the batched release freed every call").await;
    assert_eq!(s.rig.store.stats().lease_expired_calls, 0, "the release freed them, not the lease");
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// Limiter down past the lease: the entry is dropped unsent and counted; the
/// limiter's lease already freed the call.
#[tokio::test(start_paused = true)]
async fn a_release_queued_past_the_lease_is_dropped_unsent() {
    let s = Scene::new("release-queue-lease-expired", |_| {}).await;
    let mut dialog = s.establish().await;
    s.rig.expect_holds([1, 1, 1], "the call holds its three limiters").await;

    s.stall.send_replace(Stall::Releases);
    s.hang_up(&mut dialog).await;
    sip_clock::testkit::settle().await;
    assert!(s.b2bua.calls_reaped());
    s.hold_for(LEASE_SEC as u64 + 5).await;

    let metrics = s.b2bua.metrics();
    assert_eq!(metrics.limiter_release_dropped_lease_expired_total(), 1, "dropped past the lease");
    assert_eq!(metrics.limiter_release_dropped_cap_total(), 0);
    assert_eq!(s.queued(), 0, "the queue is empty");
    assert_eq!(s.rig.store.stats().lease_expired_calls, 1, "the limiter's lease freed the call");
    assert_eq!(s.rig.all_holds(), [0, 0, 0], "the server reads 0");

    let before = s.received();
    s.stall.send_replace(Stall::Nothing);
    s.hold_for(15).await;
    assert!(s.releases_since(before).is_empty(), "the dropped entry is never sent");
    s.rig.expect_drained("the lease freed the call").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// Queue at its cap: the oldest entry is dropped and counted; the others
/// leave in one request once the limiter answers, and the lease frees the
/// dropped call.
#[tokio::test(start_paused = true)]
async fn a_full_queue_drops_its_oldest_entry() {
    let s = Scene::new("release-queue-cap", |c| c.limiter_release_queue_cap = 2).await;
    let mut dialogs = Vec::new();
    for _ in 0..3 {
        dialogs.push(s.establish().await);
    }
    s.rig.expect_holds([3, 3, 3], "three calls, three limiters each").await;
    let keys = s.admitted_keys();

    s.stall.send_replace(Stall::Releases);
    for dialog in &mut dialogs {
        s.hang_up(dialog).await;
    }
    sip_clock::testkit::settle().await;
    assert!(s.b2bua.calls_reaped());
    let metrics = s.b2bua.metrics();
    assert_eq!(metrics.limiter_release_dropped_cap_total(), 1, "the oldest entry dropped");
    assert_eq!(metrics.limiter_release_dropped_lease_expired_total(), 0);
    assert_eq!(s.queued(), 2, "the queue holds its cap");

    let before = s.received();
    s.stall.send_replace(Stall::Nothing);
    s.hold_for(5).await;
    let sent = s.releases_since(before);
    assert_eq!(sent.len(), 1, "one release request: {sent:?}");
    let mut sent_keys = sent[0].clone();
    sent_keys.sort();
    let mut newest = keys[1..].to_vec();
    newest.sort();
    assert_eq!(sent_keys, newest, "the two newest calls are released, the oldest is not");
    assert_eq!(s.rig.all_holds(), [1, 1, 1], "the oldest call's set waits for its lease");

    s.hold_for(LEASE_SEC as u64).await;
    assert_eq!(s.rig.store.stats().lease_expired_calls, 1, "the lease freed the dropped call");
    s.rig.expect_drained("released or lapsed").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A call ending during a limiter outage (its admit failed open, so it owes
/// its release) writes its CDR and is removed without waiting a limiter
/// timeout.
#[tokio::test(start_paused = true)]
async fn a_call_ending_during_a_limiter_outage_is_not_delayed_by_the_limiter() {
    let s = Scene::new("release-queue-outage", |_| {}).await;
    s.stall.send_replace(Stall::Everything);
    let mut dialog = s.establish().await;
    assert_eq!(s.b2bua.limiter_count().failed_open, 1, "the admit failed open");

    s.hang_up(&mut dialog).await;
    sip_clock::testkit::settle().await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the CDR is written in the call's last turn");
    assert!(s.b2bua.calls_reaped(), "the call is removed in its last turn");
    assert_eq!(s.queued(), 1, "its release waits in the queue");

    s.stall.send_replace(Stall::Nothing);
    s.hold_for(10).await;
    settle_until(|| s.queued() == 0).await;
    assert_eq!(s.queued(), 0, "the release left once the limiter answered");
    assert!(
        s.releases_since(0).iter().any(|keys| keys == &s.admitted_keys()),
        "the call's key was released"
    );
    s.rig.expect_drained("the call never held anything").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}
