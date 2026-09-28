//! A stalled or slow limiter never delays call treatment beyond one admit
//! budget, and an open breaker delays it not at all.
//!
//! Two faults on the limiter's address: **stalled** (a request is never
//! answered) and **slow** (every request is applied and answered after the
//! admit budget, within the release and refresh budgets). Under either:
//!
//!  - an initial INVITE is forwarded within one admit budget of its arrival;
//!  - a BYE is relayed and answered in transit time: the call's CDR is written
//!    and the call removed in its last turn, its release queued;
//!  - a failover leg is dialed within one admit budget of the failure;
//!  - a call waiting on its admit delays no other call;
//!  - a refresh in flight delays an in-dialog request not at all;
//!  - once the breaker is open, INVITE, failover and BYE take transit time only;
//!  - the limiter back, every limiter drains to 0 and every call is reaped.
//!
//! Every route holds three limiters; the failover route overlaps the initial
//! one (`[x, y, z]` → `[y, z, w]`).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::config::B2buaConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallLimiterEntry, CallTreatment, NewCallResponse, RouteDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::{LimiterFailure, LimiterOp};
use b2bua_harness::B2buaSut;
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{
    BindError, Fault, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport, SimulatedHttpNetwork,
};
use scenario_harness::{
    Agent, ClientInvite, Dialog, Harness, ServerTxn, SIMULATED_TRANSIT_DELAY_MS,
};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// One SIP hop.
const HOP: Duration = Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS);
/// A request relayed through the SUT with nothing waited on: two hops.
const RELAY: Duration = Duration::from_millis(2 * SIMULATED_TRANSIT_DELAY_MS);
/// The slack a bound allows over the budget it states.
const EPS: Duration = Duration::from_millis(10);
/// The slow limiter's transit each way: an answer comes back after twice
/// this, past the admit budget and well within the release and refresh ones.
const SLOW_HOP_MS: u64 = 100;
/// The breaker's default probe period.
const PROBE: Duration = Duration::from_secs(1);
/// The refresh period the long-call scenario runs.
const REFRESH_SEC: i64 = 5;
/// How long the limiter's return may take to drain every release.
const DRAIN_BOUND: Duration = Duration::from_secs(10);
/// The initial route's limiters.
const INITIAL: &[&str] = &["x", "y", "z"];
/// The failover route's limiters: `y` and `z` kept, `x` dropped, `w` added.
const FAILOVER: &[&str] = &["y", "z", "w"];
/// Every id either route holds, in the order [`Scene::holds`] reads them.
const IDS: [&str; 4] = ["x", "y", "z", "w"];
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// The fault the limiter runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limiter {
    /// No request is ever answered until the limiter is back.
    Stalled,
    /// Every request is applied and answered after the admit budget.
    Slow,
}

impl Limiter {
    fn fault(self) -> Fault {
        match self {
            Self::Stalled => Fault::Stall { dst: laddr() },
            Self::Slow => Fault::Delay { dst: laddr(), ms: SLOW_HOP_MS },
        }
    }
}

/// One request the SUT's limiter client sent: its path and its JSON body.
type Sent = (String, serde_json::Value);

/// The simulated fabric behind a log of every request the client sends,
/// whether or not a fault lets it reach the limiter.
struct Recording {
    net: SimulatedHttpNetwork,
    sent: Arc<Mutex<Vec<Sent>>>,
}

#[async_trait]
impl HttpTransport for Recording {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        self.net.serve(addr, service).await
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let body = serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
        self.sent.lock().unwrap().push((req.path.clone(), body));
        self.net.request(dst, req).await
    }
}

fn limited_route(port: u16, ids: &[&str]) -> RouteDecision {
    let mut r = route_to("127.0.0.1", port);
    r.callback_context = Some("failover-ctx".into());
    r.call_limiter =
        ids.iter().map(|id| CallLimiterEntry { id: (*id).into(), limit: 10 }).collect();
    r
}

/// A scenario: a limiter store served on a simulated fabric, the SUT's
/// production client on it behind [`Recording`]. Every call is routed to bob
/// holding [`INITIAL`]; a failure of bob's leg fails over to carol holding
/// [`FAILOVER`].
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    carol: Agent,
    limiter: Limiter,
    net: SimulatedHttpNetwork,
    store: Arc<CallStore>,
    sent: Arc<Mutex<Vec<Sent>>>,
    b2bua: B2buaSut,
    _server: Box<dyn HttpServerHandle>,
}

impl Scene {
    async fn new(name: &str, limiter: Limiter) -> Self {
        Self::tuned(name, limiter, |_| {}).await
    }

    async fn tuned(
        name: &str,
        limiter: Limiter,
        tune: impl FnOnce(&mut B2buaConfig) + 'static,
    ) -> Self {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let carol = h.agent("carol", "127.0.0.1:5071").await;
        let net = SimulatedHttpNetwork::new();
        let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let handle = net.serve(laddr(), server).await.expect("limiter binds");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(Recording { net: net.clone(), sent: sent.clone() });
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(transport, laddr(), ADMIT_BUDGET));
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| NewCallResponse::Route(limited_route(5070, INITIAL)))
                .on_failure(|_| CallTreatment::Route(limited_route(5071, FAILOVER)))
                .build(),
        );
        let b2bua = B2buaSut::builder(decision)
            .limiter(client)
            .limiter_store(store.clone())
            .tune(move |c| {
                c.keepalive_interval_sec = 3_600;
                tune(c);
            })
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        Self { h, alice, bob, carol, limiter, net, store, sent, b2bua, _server: handle }
    }

    /// Put the limiter under its fault.
    fn fail(&self) {
        self.net.apply_fault(self.limiter.fault());
    }

    /// `caller`'s INVITE through the SUT, received by bob. Returns both
    /// transactions and how long the INVITE took from caller to callee.
    async fn invite(&self, caller: &Agent) -> (ClientInvite, ServerTxn, Duration) {
        let start = Instant::now();
        let call = caller.invite(&self.bob).with_sdp(OFFER).through(self.b2bua.addr).send().await;
        let uas = self.bob.receive("INVITE").await;
        (call, uas, start.elapsed())
    }

    /// `callee` answers the INVITE `uas` received; the caller ACKs.
    async fn answer(&self, mut call: ClientInvite, mut uas: ServerTxn, callee: &Agent) -> Dialog {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        callee.receive("ACK").await;
        dialog
    }

    /// INVITE → 200 → ACK from `caller` to bob. Returns the dialog and how
    /// long the INVITE took from caller to callee.
    async fn establish(&self, caller: &Agent) -> (Dialog, Duration) {
        let (call, uas, took) = self.invite(caller).await;
        (self.answer(call, uas, &self.bob).await, took)
    }

    /// The caller's BYE: the SUT answers it and relays it to `callee`, whose
    /// 200 then reaches the SUT (the call's last turn). Returns how long the
    /// BYE took to reach the callee and its 200 to reach the caller.
    async fn hang_up(&self, dialog: &mut Dialog, callee: &Agent) -> (Duration, Duration) {
        let start = Instant::now();
        let mut bye = dialog.bye().await;
        let mut uas = callee.receive("BYE").await;
        let relayed = start.elapsed();
        bye.expect(200).await;
        let answered = start.elapsed();
        uas.respond(200, "OK").await;
        b2bua_harness::advance(SIMULATED_TRANSIT_DELAY_MS).await;
        (relayed, answered)
    }

    /// A caller's re-INVITE (SDP version `version`) answered by bob and
    /// ACKed. Returns how long it took from caller to callee.
    async fn reinvite(&self, dialog: &mut Dialog, version: u32) -> Duration {
        let start = Instant::now();
        let offer = OFFER.replace("o=alice 1 1", &format!("o=alice 1 {version}"));
        let mut reinv = dialog.request(InDialogMethod::Invite, Some(&offer)).await;
        let mut uas = self.bob.receive("INVITE").await;
        let took = start.elapsed();
        let answer = ANSWER.replace("o=bob 1 1", &format!("o=bob 1 {version}"));
        uas.respond(200, "OK").with_sdp(&answer).await;
        reinv.expect(200).await;
        dialog.ack(None).await;
        self.bob.receive("ACK").await;
        took
    }

    /// Requests the client sent on `path` so far.
    fn sent_on(&self, path: &str) -> usize {
        self.sent.lock().unwrap().iter().filter(|(p, _)| p == path).count()
    }

    /// The distinct limiter keys every admit sent named, sorted.
    fn admitted_keys(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        distinct(
            sent.iter()
                .filter(|(p, _)| p == "/v1/admit")
                .map(|(_, body)| body["key"].as_str().expect("an admit names its key").into())
                .collect(),
        )
    }

    /// The distinct keys every release sent named, sorted.
    fn released_keys(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        distinct(
            sent.iter()
                .filter(|(p, _)| p == "/v1/release")
                .flat_map(|(_, body)| {
                    body["keys"]
                        .as_array()
                        .expect("a release names its keys")
                        .iter()
                        .map(|k| k.as_str().expect("a key is a string").to_string())
                        .collect::<Vec<_>>()
                })
                .collect(),
        )
    }

    /// The live count of every id of [`IDS`], in order.
    fn holds(&self) -> [i64; 4] {
        IDS.map(|id| self.store.held(id))
    }

    /// The call whose BYE's 200 was just relayed wrote its CDR and was
    /// removed in its last turn, its release left waiting: `ended` CDRs
    /// written so far, `live` calls left, `waiting` releases unanswered.
    async fn assert_ended(&self, ended: usize, live: usize, waiting: usize) {
        sip_clock::testkit::settle().await;
        assert_eq!(self.b2bua.cdr_records().len(), ended, "the CDR is written in the last turn");
        assert_eq!(self.b2bua.active_calls(), live, "the call is removed in its last turn");
        assert_eq!(self.b2bua.limiter_releases_waiting(), waiting, "releases waiting");
    }

    /// The limiter is back: every release lands within [`DRAIN_BOUND`], every
    /// limiter drains to 0, only a call that sent an admit releases, and the
    /// releases freed the calls, not the lease. Then the reaped check.
    async fn back_and_drained(&self) {
        self.net.apply_fault(Fault::Resume { dst: laddr() });
        let deadline = Instant::now() + DRAIN_BOUND;
        while !(self.b2bua.is_reaped() && self.store.stats().current_total == 0) {
            assert!(Instant::now() < deadline, "not drained within {DRAIN_BOUND:?}");
            b2bua_harness::advance(100).await;
        }
        assert_eq!(self.holds(), [0; 4], "every limiter drained to 0");
        assert_eq!(self.released_keys(), self.admitted_keys(), "every call that admitted released");
        assert_eq!(self.store.stats().lease_expired_calls, 0, "released, not lapsed");
        self.b2bua.assert_fully_reaped();
    }
}

/// The distinct values of `keys`, sorted.
fn distinct(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys.dedup();
    keys
}

/// `took` is one relay plus the admit budget the faulty limiter made it wait,
/// and no more.
#[track_caller]
fn within_one_admit(took: Duration, what: &str) {
    assert!(took >= RELAY + ADMIT_BUDGET, "{what} took {took:?}: no admit waited on");
    assert!(
        took <= RELAY + ADMIT_BUDGET + EPS,
        "{what} took {took:?}: more than one admit budget over its relay"
    );
}

#[track_caller]
fn no_limiter_time(took: Duration, what: &str) {
    assert_eq!(took, RELAY, "{what} waited on the limiter");
}

// ── initial INVITE ──────────────────────────────────────────────────────────

/// The INVITE waits its admit budget and no longer; the call runs uncounted,
/// owes its release (its admit may have landed), and drains once the limiter
/// is back.
async fn initial_invite_is_forwarded_within_one_admit_budget(limiter: Limiter) {
    let s = Scene::new(&format!("limiter-{limiter:?}-initial-invite"), limiter).await;
    s.fail();
    let (mut dialog, took) = s.establish(&s.alice).await;
    within_one_admit(took, "the INVITE");
    assert_eq!(s.b2bua.limiter_count().failed_open, 1, "the admit failed open");

    let (bye, ok) = s.hang_up(&mut dialog, &s.bob).await;
    no_limiter_time(bye, "the BYE");
    no_limiter_time(ok, "the BYE's 200");
    s.assert_ended(1, 0, 1).await;
    s.back_and_drained().await;
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_initial_invite_is_forwarded_within_one_admit_budget() {
    initial_invite_is_forwarded_within_one_admit_budget(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_initial_invite_is_forwarded_within_one_admit_budget() {
    initial_invite_is_forwarded_within_one_admit_budget(Limiter::Slow).await;
}

// ── BYE ─────────────────────────────────────────────────────────────────────

/// A counted call ends on a faulty limiter: the BYE and its 200 are relayed
/// in transit time, the CDR is written and the call removed in its last
/// turn, and the release waits in the queue.
async fn bye_never_waits_on_the_release(limiter: Limiter) {
    let s = Scene::new(&format!("limiter-{limiter:?}-bye"), limiter).await;
    let (mut dialog, _) = s.establish(&s.alice).await;
    assert_eq!(s.holds(), [1, 1, 1, 0], "the call is counted");

    s.fail();
    let (bye, ok) = s.hang_up(&mut dialog, &s.bob).await;
    no_limiter_time(bye, "the BYE");
    no_limiter_time(ok, "the BYE's 200");
    s.assert_ended(1, 0, 1).await;
    if limiter == Limiter::Stalled {
        assert_eq!(s.holds(), [1, 1, 1, 0], "the stalled limiter applied nothing");
    }
    s.back_and_drained().await;
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_bye_never_waits_on_the_release() {
    bye_never_waits_on_the_release(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_bye_never_waits_on_the_release() {
    bye_never_waits_on_the_release(Limiter::Slow).await;
}

// ── failover ────────────────────────────────────────────────────────────────

/// Bob busies out on a faulty limiter: carol's leg is dialed within one
/// admit budget of bob's 486. The replacing admit gets no answer, so the
/// counted call stays counted: on its old set when the admit never landed
/// (stalled), on the new one when it landed (slow). Its release frees
/// whichever set the limiter holds.
async fn failover_leg_is_dialed_within_one_admit_budget(limiter: Limiter) {
    let s = Scene::new(&format!("limiter-{limiter:?}-failover"), limiter).await;
    let (mut call, mut bob_uas, _) = s.invite(&s.alice).await;
    assert_eq!(s.holds(), [1, 1, 1, 0], "the call is counted on the initial route");

    s.fail();
    let failed = Instant::now();
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    let mut carol_uas = s.carol.receive("INVITE").await;
    within_one_admit(failed.elapsed(), "the failover INVITE");

    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.carol.receive("ACK").await;
    let expected = match limiter {
        Limiter::Stalled => [1, 1, 1, 0],
        Limiter::Slow => [0, 1, 1, 1],
    };
    assert_eq!(s.holds(), expected, "the set the limiter holds for the call");

    let (bye, ok) = s.hang_up(&mut dialog, &s.carol).await;
    no_limiter_time(bye, "the BYE");
    no_limiter_time(ok, "the BYE's 200");
    s.assert_ended(1, 0, 1).await;
    s.back_and_drained().await;
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_failover_leg_is_dialed_within_one_admit_budget() {
    failover_leg_is_dialed_within_one_admit_budget(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_failover_leg_is_dialed_within_one_admit_budget() {
    failover_leg_is_dialed_within_one_admit_budget(Limiter::Slow).await;
}

// ── concurrent calls ────────────────────────────────────────────────────────

/// Dave's INVITE waits its admit; 50 ms later erin's INVITE arrives and
/// alice hangs up her counted call. Erin's INVITE waits its own admit budget,
/// not dave's plus its own, and alice's BYE waits nothing.
async fn a_call_waiting_on_its_admit_delays_no_other_call(limiter: Limiter) {
    let s = Scene::new(&format!("limiter-{limiter:?}-concurrent"), limiter).await;
    let dave = s.h.agent("dave", "127.0.0.1:5061").await;
    let erin = s.h.agent("erin", "127.0.0.1:5062").await;
    let (mut counted, _) = s.establish(&s.alice).await;

    s.fail();
    let dave_sent = Instant::now();
    let dave_call = dave.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    b2bua_harness::advance(50).await;
    let erin_sent = Instant::now();
    let erin_call = erin.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut bye = counted.bye().await;

    // Arrivals at bob, in order: alice's BYE, dave's INVITE, erin's INVITE.
    let mut bye_uas = s.bob.receive("BYE").await;
    no_limiter_time(erin_sent.elapsed(), "alice's BYE");
    bye_uas.respond(200, "OK").await;
    let dave_uas = s.bob.receive("INVITE").await;
    within_one_admit(dave_sent.elapsed(), "dave's INVITE");
    let erin_uas = s.bob.receive("INVITE").await;
    within_one_admit(erin_sent.elapsed(), "erin's INVITE");
    bye.expect(200).await;
    s.assert_ended(1, 2, 1).await;

    let mut dave_dialog = s.answer(dave_call, dave_uas, &s.bob).await;
    let mut erin_dialog = s.answer(erin_call, erin_uas, &s.bob).await;
    assert!(!s.b2bua.metrics().limiter().breaker_open(), "two failed admits keep it closed");
    s.hang_up(&mut dave_dialog, &s.bob).await;
    s.hang_up(&mut erin_dialog, &s.bob).await;
    s.back_and_drained().await;
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_a_call_waiting_on_its_admit_delays_no_other_call() {
    a_call_waiting_on_its_admit_delays_no_other_call(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_a_call_waiting_on_its_admit_delays_no_other_call() {
    a_call_waiting_on_its_admit_delays_no_other_call(Limiter::Slow).await;
}

// ── refresh ─────────────────────────────────────────────────────────────────

/// A counted call's refresh leaves on a faulty limiter; the caller's
/// re-INVITE sent while it is in flight is relayed exactly as fast as on the
/// healthy limiter, and so is the BYE.
async fn a_refresh_in_flight_delays_no_in_dialog_request(limiter: Limiter) {
    let s = Scene::tuned(&format!("limiter-{limiter:?}-refresh"), limiter, |c| {
        c.limiter_refresh_sec = REFRESH_SEC;
    })
    .await;
    let (mut dialog, _) = s.establish(&s.alice).await;
    let counted_at = Instant::now();
    let healthy = s.reinvite(&mut dialog, 2).await;
    no_limiter_time(healthy, "the re-INVITE on the healthy limiter");

    let quiet = counted_at + Duration::from_secs(REFRESH_SEC as u64) - 10 * HOP;
    b2bua_harness::advance((quiet - Instant::now()).as_millis() as u64).await;
    assert_eq!(s.sent_on("/v1/refresh"), 0, "no refresh left yet");
    s.fail();
    let deadline = Instant::now() + Duration::from_secs(3);
    while s.sent_on("/v1/refresh") == 0 {
        assert!(Instant::now() < deadline, "no refresh request left");
        b2bua_harness::advance(10).await;
    }
    let took = s.reinvite(&mut dialog, 3).await;
    assert_eq!(took, healthy, "the re-INVITE waited on the refresh in flight");
    assert_eq!(s.holds(), [1, 1, 1, 0], "the call stays counted");

    let (bye, ok) = s.hang_up(&mut dialog, &s.bob).await;
    no_limiter_time(bye, "the BYE");
    no_limiter_time(ok, "the BYE's 200");
    s.assert_ended(1, 0, 1).await;
    s.back_and_drained().await;
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_a_refresh_in_flight_delays_no_in_dialog_request() {
    a_refresh_in_flight_delays_no_in_dialog_request(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_a_refresh_in_flight_delays_no_in_dialog_request() {
    a_refresh_in_flight_delays_no_in_dialog_request(Limiter::Slow).await;
}

// ── open breaker ────────────────────────────────────────────────────────────

/// Three admits without an answer open the breaker. From then on an INVITE,
/// a failover and a BYE take transit time only and send nothing to the
/// limiter; the releases owed wait, held. Within one probe period of the
/// limiter's return the breaker closes and every release leaves.
async fn an_open_breaker_costs_no_limiter_time(limiter: Limiter) {
    let s = Scene::new(&format!("limiter-{limiter:?}-breaker-open"), limiter).await;
    let (mut counted, _) = s.establish(&s.alice).await;

    s.fail();
    let mut outage = Vec::new();
    for n in 1..=3 {
        let (dialog, took) = s.establish(&s.alice).await;
        within_one_admit(took, &format!("outage call {n}'s INVITE"));
        outage.push(dialog);
    }
    let metrics = s.b2bua.metrics();
    assert!(metrics.limiter().breaker_open(), "three admits without an answer open the breaker");
    let admits = s.sent_on("/v1/admit");

    let (mut call, mut bob_uas, took) = s.invite(&s.alice).await;
    no_limiter_time(took, "the INVITE");
    let failed = Instant::now();
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    let mut carol_uas = s.carol.receive("INVITE").await;
    no_limiter_time(failed.elapsed(), "the failover INVITE");
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut rerouted = call.ack().await;
    s.carol.receive("ACK").await;
    assert_eq!(s.sent_on("/v1/admit"), admits, "no admit request while open");
    assert_eq!(
        metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen),
        2,
        "the INVITE's and the failover's"
    );

    let (bye, ok) = s.hang_up(&mut counted, &s.bob).await;
    no_limiter_time(bye, "the BYE");
    no_limiter_time(ok, "the BYE's 200");
    s.assert_ended(1, 4, 1).await;
    let (bye, ok) = s.hang_up(&mut rerouted, &s.carol).await;
    no_limiter_time(bye, "the rerouted call's BYE");
    no_limiter_time(ok, "the rerouted call's 200");
    s.assert_ended(2, 3, 1).await;
    for dialog in &mut outage {
        s.hang_up(dialog, &s.bob).await;
    }
    s.assert_ended(5, 0, 4).await;
    assert_eq!(s.sent_on("/v1/release"), 0, "the open breaker holds every release");

    s.net.apply_fault(Fault::Resume { dst: laddr() });
    b2bua_harness::advance((PROBE + Duration::from_millis(200)).as_millis() as u64).await;
    assert!(!metrics.limiter().breaker_open(), "the probe closed the breaker");
    assert_eq!(s.b2bua.limiter_releases_waiting(), 0, "every release left on close");
    s.back_and_drained().await;
    assert_eq!(s.admitted_keys().len(), 4, "the counted call and the three outage calls");
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_limiter_an_open_breaker_costs_no_limiter_time() {
    an_open_breaker_costs_no_limiter_time(Limiter::Stalled).await;
}

#[tokio::test(start_paused = true)]
async fn slow_limiter_an_open_breaker_costs_no_limiter_time() {
    an_open_breaker_costs_no_limiter_time(Limiter::Slow).await;
}
