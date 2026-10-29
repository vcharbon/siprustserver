//! A decision consult the core sends off the call's turn whose answer dies
//! with the node that sent it: the node crashes with the consult in flight,
//! and the call is served again either by the rebooted primary's reclaim or by
//! the backup's takeover on a message that reaches it (the proxy tells no dead
//! verdict), or by the backup's takeover at the proxy's dead verdict on a
//! primary killed unannounced. The node serving the call again sends the consult again from its
//! record (ADR-0039); the engine under test never answers a consult, so what
//! bounds the wait is the re-sent consult's own:
//!
//! - a `/call/failure` consult: the decision deadline of the re-sent consult,
//!   past which the caller is answered the stack's own `503`, as for an
//!   unanswered consult;
//! - a release event's consult (the subscribed maximum duration): the
//!   re-sent consult's decision deadline, past which the call ends locally;
//! - a REFER's authorization: its subscription's expiry, a timer of the
//!   call's own; the referrer is told the transfer failed.
//!
//! Each call ends with one CDR and the limiter drained.

use call::LimiterEntry;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::answer_deadline::{failure_budget, release_budget, MARGIN};
use b2bua::decision::test_adapter::route_to_processing_refer;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse,
};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use call::{ReleaseEventKind, TerminationCause};
use call_limiter::{CallStore, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, cookie_field, total_cdrs_for, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth, LEASE_OUTLIVING_THE_REPLICA_TTL,
};
use http_net::{HttpServerHandle, HttpTransport};
use http_net_testkit::SimulatedHttpNetwork;
use scenario_harness::{Agent, Dialog};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The decision engine's per-consult deadline (`call_control_timeout_ms`).
const CONSULT_BUDGET: Duration = Duration::from_secs(2);
/// The limiter client's admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(500);
/// A maximum duration subscribed to its release event, short enough to fire
/// in the release cells.
const SHORT_DURATION_SEC: i64 = 10;
/// A maximum duration no cell reaches: the call-level duration timer armed
/// at route time does not end a call in setup before the failure deadline.
const LONG_DURATION_SEC: i64 = 3600;
/// The REFER subscription's expiry.
const REFER_EXPIRY_SEC: i64 = 6;
/// The clock step while waiting for a consult to leave.
const STEP: Duration = Duration::from_millis(100);

fn limiter_addr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// Routes every call to bob holding `trunk`, subscribed to its maximum
/// duration's release event, REFER processed locally; answers no consult.
struct Unanswering {
    max_duration_sec: i64,
    asked: Arc<AtomicUsize>,
}

#[async_trait]
impl CallDecisionEngine for Unanswering {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        let mut r = route_to_processing_refer("127.0.0.1", 5070);
        r.new_ruri = None;
        r.call_limiter = vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
        r.callback_context = Some("ctx".into());
        r.features.platform.max_duration_ms = self.max_duration_sec * 1000;
        r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
        Ok(NewCallResponse::Route(r))
    }

    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }

    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }

    async fn call_release(
        &self,
        _req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    /// Consults the engine was asked, on either node.
    asked: Arc<AtomicUsize>,
    store: Arc<CallStore>,
    _limiter: Box<dyn HttpServerHandle>,
}

/// Two replicating workers behind the proxy over one unanswering engine
/// routing with `max_duration_sec`, and one limiter server; the proxy tells
/// no dead verdict.
async fn cluster(name: &str, max_duration_sec: i64) -> Cluster {
    cluster_telling(name, max_duration_sec, false).await
}

/// [`cluster`], the proxy telling its dead verdicts when `tells`.
async fn cluster_telling(name: &str, max_duration_sec: i64, tells: bool) -> Cluster {
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LEASE_OUTLIVING_THE_REPLICA_TTL, Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _limiter: Box<dyn HttpServerHandle> = http.serve(limiter_addr(), server).await.unwrap();
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), limiter_addr(), ADMIT_BUDGET));
    let asked = Arc::new(AtomicUsize::new(0));
    let engine: Arc<dyn CallDecisionEngine> =
        Arc::new(Unanswering { max_duration_sec, asked: asked.clone() });
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_tune(|c| {
        c.call_control_timeout_ms = CONSULT_BUDGET.as_millis() as i64;
        c.refer_subscription_expiry_sec = REFER_EXPIRY_SEC;
    });
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let mut proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    if !tells {
        proxy.tell_no_dead_verdict();
    }
    let mut spawn = async |ordinal: &str, addr: &str, peer: &str| {
        fh.spawn_worker_limited(
            ordinal,
            ordinal,
            addr,
            &[peer],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            engine.clone(),
            limiter.clone(),
        )
        .await
    };
    let w_b1 = spawn("b1", B1, "b2").await;
    let w_b2 = spawn("b2", B2, "b1").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");
    Cluster { fh, alice, bob, proxy, w_b1, w_b2, asked, store, _limiter }
}

fn worker<'c>(c: &'c mut Cluster, ordinal: &str) -> &'c mut ReplicatedB2buaSut {
    if ordinal == "b1" {
        &mut c.w_b1
    } else {
        &mut c.w_b2
    }
}

fn backup_of(primary: &str) -> &'static str {
    if primary == "b1" {
        "b2"
    } else {
        "b1"
    }
}

/// How the call is served once its primary crashed with the consult out.
#[derive(Clone, Copy)]
enum Recovery {
    /// The primary reboots and reclaims the call.
    Reclaim,
    /// The primary stays dead; the next in-dialog request makes the backup
    /// take the call over. The primary reboots once the copy ended the call,
    /// and discharges it.
    Takeover,
}

/// An established alice ↔ bob call.
struct Established {
    dialog: Dialog,
    bob_dialog: Dialog,
    call_ref: String,
    primary: String,
}

async fn establish(c: &mut Cluster) -> Established {
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let primary = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    c.bob.receive("ACK").await;
    c.fh.advance(Duration::from_millis(500)).await;
    let call_ref = served_call(c, &primary);
    Established { dialog, bob_dialog: uas.dialog(), call_ref, primary }
}

fn served_call(c: &mut Cluster, primary: &str) -> String {
    worker(c, primary)
        .scan_primary(primary)
        .into_iter()
        .next()
        .expect("the primary serves the call")
}

/// The clock runs until the engine has been asked `n` consults: the turn
/// that sent the last one lies within one [`STEP`] before the instant
/// returned.
async fn consults_sent(c: &mut Cluster, n: usize) -> i64 {
    for _ in 0..200 {
        if c.asked.load(Ordering::SeqCst) >= n {
            return c.fh.now_ms();
        }
        c.fh.advance(STEP).await;
    }
    panic!("consult {n} was not sent");
}

/// The primary crashes (the consult dies with it) and the proxy reads it dead;
/// the asking turn replicated first.
async fn crash(c: &mut Cluster, primary: &str) {
    c.fh.advance(Duration::from_millis(300)).await;
    worker(c, primary).crash();
    c.proxy.set_health(primary, WorkerHealth::Dead);
    c.fh.advance(Duration::from_millis(300)).await;
}

/// The crashed primary reboots, becomes ready and is announced to the proxy.
async fn reboot(c: &mut Cluster, primary: &str) {
    let new_addr = worker(c, primary).reboot().await;
    for _ in 0..40 {
        c.fh.advance(Duration::from_millis(500)).await;
        if worker(c, primary).is_ready() {
            break;
        }
    }
    assert!(worker(c, primary).is_ready(), "rebooted primary re-hydrated from the backup");
    c.proxy.set_address(primary, new_addr);
    c.fh.note_worker_rebound(primary, new_addr);
    c.proxy.set_health(primary, WorkerHealth::Alive);
    c.fh.advance(Duration::from_millis(500)).await;
}

/// [`reboot`], and the primary reclaims its call.
async fn reboot_and_reclaim(c: &mut Cluster, primary: &str) {
    reboot(c, primary).await;
    assert_eq!(worker(c, primary).active_calls(), 1, "the reboot reclaim re-materialised the call");
}

/// The copy that took the ended call over lets it go once its transactions
/// clear (the non-INVITE ones linger 64·T1, RFC 3261 §17.2.2); the rebooted
/// primary discharges it.
async fn copy_lets_go_then_reboot(c: &mut Cluster, primary: &str, call_ref: &str) {
    let backup = backup_of(primary);
    for _ in 0..160 {
        if !worker(c, backup).serves(call_ref) {
            break;
        }
        c.fh.advance(Duration::from_millis(500)).await;
    }
    assert!(!worker(c, backup).serves(call_ref), "the copy let the call go");
    reboot(c, primary).await;
}

/// The window the lost answer is read in: the consult left within one
/// [`STEP`] before `asked_at` and is answered by `deadline` past it.
fn window(asked_at: i64, deadline: Duration) -> (i64, i64) {
    let earliest = asked_at - STEP.as_millis() as i64 + deadline.as_millis() as i64;
    (earliest, asked_at + deadline.as_millis() as i64 + STEP.as_millis() as i64)
}

fn assert_in(c: &Cluster, (earliest, latest): (i64, i64), what: &str) {
    let now = c.fh.now_ms();
    assert!(now >= earliest, "{what} before the consult's deadline ({now} < {earliest})");
    assert!(now <= latest, "{what} past the consult's deadline ({now} > {latest})");
}

/// The call is over everywhere with one CDR and nothing held; the CDR's
/// termination cause.
async fn fully_over(c: &mut Cluster, call_ref: &str) -> TerminationCause {
    let (b1, b2) = (&c.w_b1, &c.w_b2);
    c.fh.settle_terminal(async || {
        let mut over = true;
        for n in [b1, b2] {
            over &= !n.holds_any_trace(call_ref).await && n.memory_clean();
        }
        over
    })
    .await;
    c.fh.linger_peers(&[&c.alice, &c.bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&c.w_b1, &c.w_b2], call_ref), 1, "one CDR");
    assert_call_fully_over(&[&c.w_b1, &c.w_b2], call_ref, &c.store).await;
    assert_eq!(c.store.stats().current_total, 0, "the call's release drained the limiter");
    let cdr = [&c.w_b1, &c.w_b2]
        .iter()
        .flat_map(|n| n.cdr_records())
        .find(|r| r.call_ref == call_ref)
        .expect("the one CDR");
    cdr.termination.expect("the record names what ended the call").cause
}

// ── /call/failure: sent again, its decision deadline ───────────────────────

/// Bob rings then refuses alice's call; the `/call/failure` consult is lost
/// with the primary; the node serving the call again sends it again at once,
/// and answers alice the stack's own 503 at that consult's decision deadline,
/// before the lost one's answer deadline. On the takeover path alice's UPDATE
/// on the early dialog makes the backup take the call over; the pending
/// failover answers it 491 (RFC 5407 §3.1).
async fn failure_cell(name: &str, recovery: Recovery) {
    let mut c = cluster(name, LONG_DURATION_SEC).await;
    let mut invite = c.alice.invite(&c.bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let primary = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(180, "Ringing").await;
    invite.expect(180).await;
    uas.respond(486, "Busy Here").await;
    c.bob.receive_absorbing("ACK", &["INVITE"]).await;
    let asked_at = consults_sent(&mut c, 1).await;
    let call_ref = served_call(&mut c, &primary);
    crash(&mut c, &primary).await;
    let budget = failure_budget(CONSULT_BUDGET, ADMIT_BUDGET).expect("a bounded consult");
    let lost_deadline = window(asked_at, budget + MARGIN);
    match recovery {
        Recovery::Reclaim => reboot_and_reclaim(&mut c, &primary).await,
        Recovery::Takeover => {
            invite.send_request(InDialogMethod::Update).send().await.expect(491).await;
            assert!(worker(&mut c, backup_of(&primary)).serves(&call_ref), "the backup took over");
        }
    }
    // The reclaim re-sends while the rebooted node turns ready, the copy as
    // it takes the call over: by the time the call is served either way.
    let served_at = c.fh.now_ms();
    let resent_at = consults_sent(&mut c, 2).await;
    assert!(resent_at <= served_at + STEP.as_millis() as i64, "re-sent as the call is served");
    invite.expect(503).await;
    let answered_by = served_at + (CONSULT_BUDGET + STEP).as_millis() as i64;
    assert!(c.fh.now_ms() <= answered_by, "answered within the re-sent consult's deadline");
    assert!(c.fh.now_ms() < lost_deadline.0, "before the lost consult's answer deadline");
    assert_eq!(c.asked.load(Ordering::SeqCst), 2, "the lost consult is sent again once");
    if let Recovery::Takeover = recovery {
        copy_lets_go_then_reboot(&mut c, &primary, &call_ref).await;
    }
    assert_eq!(fully_over(&mut c, &call_ref).await, TerminationCause::Admission);
}

#[tokio::test(start_paused = true)]
async fn a_failure_consult_lost_with_its_node_is_sent_again_by_the_reclaimed_call() {
    failure_cell("consult-answer-deadline-failure", Recovery::Reclaim).await;
}

#[tokio::test(start_paused = true)]
async fn a_failure_consult_lost_with_its_node_is_sent_again_by_the_takeover_copy() {
    failure_cell("consult-answer-deadline-failure-takeover", Recovery::Takeover).await;
}

// ── release event: sent again, its decision deadline ───────────────────────

/// The backup takes the call over on bob's in-dialog INFO, relayed to alice.
async fn take_over(c: &mut Cluster, call: &mut Established) {
    let mut ping = call
        .bob_dialog
        .send_request(InDialogMethod::Info)
        .with_body("text/plain", b"ping".to_vec())
        .send()
        .await;
    let mut relayed = c.alice.receive("INFO").await;
    relayed.respond(200, "OK").await;
    ping.expect(200).await;
    assert!(worker(c, backup_of(&call.primary)).serves(&call.call_ref), "the backup took over");
}

/// The subscribed maximum duration fires and its release consult is lost
/// with the primary. The node that serves the call again sends the consult
/// again at once (the decision layer sees the release event twice);
/// unanswered within its decision deadline, it ends the call locally.
async fn release_cell(name: &str, recovery: Recovery) {
    let mut c = cluster(name, SHORT_DURATION_SEC).await;
    let mut call = establish(&mut c).await;
    let asked_at = consults_sent(&mut c, 1).await;
    let primary = call.primary.clone();
    crash(&mut c, &primary).await;
    match recovery {
        Recovery::Reclaim => reboot_and_reclaim(&mut c, &primary).await,
        Recovery::Takeover => take_over(&mut c, &mut call).await,
    }
    let served_at = c.fh.now_ms();
    let budget = release_budget(CONSULT_BUDGET, ADMIT_BUDGET).expect("a bounded consult");
    let deadline = asked_at + (budget + MARGIN).as_millis() as i64;
    let resent_at = consults_sent(&mut c, 2).await;
    assert!(
        resent_at <= served_at + STEP.as_millis() as i64,
        "the node serving the call re-sent the release consult as it was served"
    );
    assert!(resent_at < deadline, "before the lost consult's answer deadline");
    c.alice.receive_absorbing("BYE", &["INFO"]).await.respond(200, "OK").await;
    c.bob.receive_absorbing("BYE", &["INFO"]).await.respond(200, "OK").await;
    assert_eq!(c.asked.load(Ordering::SeqCst), 2, "the copy re-sent the release consult once");
    let ended_by = resent_at + (CONSULT_BUDGET + STEP).as_millis() as i64;
    assert!(c.fh.now_ms() <= ended_by, "the call ended within the re-sent consult's deadline");
    if let Recovery::Takeover = recovery {
        copy_lets_go_then_reboot(&mut c, &primary, &call.call_ref).await;
    }
    assert_eq!(fully_over(&mut c, &call.call_ref).await, TerminationCause::MaxDuration);
}

#[tokio::test(start_paused = true)]
async fn a_release_consult_lost_with_its_node_is_re_sent_by_the_reclaimed_call() {
    release_cell("consult-answer-deadline-release", Recovery::Reclaim).await;
}

#[tokio::test(start_paused = true)]
async fn a_release_consult_lost_with_its_node_is_re_sent_by_the_takeover_copy() {
    release_cell("consult-answer-deadline-release-takeover", Recovery::Takeover).await;
}

// ── REFER: the subscription's expiry ────────────────────────────────────────

/// A REFER's authorization lost with its node: the reclaimed call sends it
/// again, and unanswered, tells the referrer the transfer failed at the
/// subscription's expiry, and runs on.
#[tokio::test(start_paused = true)]
async fn a_refer_consult_lost_with_its_node_is_sent_again_and_ends_at_the_subscription_expiry() {
    let mut c = cluster("consult-answer-deadline-refer", LONG_DURATION_SEC).await;
    let mut call = establish(&mut c).await;
    let refer_at = c.fh.now_ms();
    let mut refer = call
        .bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", "<sip:charlie@127.0.0.1:5090>")
        .send()
        .await;
    refer.expect(202).await;
    c.bob.receive_absorbing("NOTIFY", &["ACK"]).await.respond(200, "OK").await;
    consults_sent(&mut c, 1).await;
    let primary = call.primary.clone();
    crash(&mut c, &primary).await;
    reboot_and_reclaim(&mut c, &primary).await;
    let expiry = refer_at + REFER_EXPIRY_SEC * 1000;
    assert!(c.fh.now_ms() < expiry, "the call is served again before the expiry");
    let mut notify = c.bob.receive_absorbing("NOTIFY", &["ACK"]).await;
    assert!(
        String::from_utf8_lossy(notify.request().body()).starts_with("SIP/2.0 500"),
        "the referrer is told the transfer failed"
    );
    notify.respond(200, "OK").await;
    assert_in(&c, window(refer_at, Duration::from_secs(REFER_EXPIRY_SEC as u64)), "told");
    assert_eq!(c.asked.load(Ordering::SeqCst), 2, "the authorization is sent again once");
    scenario_harness::callflow::hangup(&mut call.dialog, &c.bob).await;
    assert_eq!(fully_over(&mut c, &call.call_ref).await, TerminationCause::RemoteBye);
}

/// How long the proxy's probe takes to read a node killed unannounced dead
/// (ADR-0031 case 3a: two missed windows).
const PROBE_READS_DEAD_AFTER: Duration = Duration::from_secs(2);

/// A REFER's authorization lost with a primary killed unannounced: the
/// replication stream from it ends with the node, so the proxy's dead verdict
/// is confirmed and the backup takes the call over at once, sending the
/// authorization again before the lost one is due. Unanswered, the referrer
/// is told the transfer failed at the subscription's expiry; the call runs
/// on and ends with one CDR once the node's endpoint leaves the slice.
#[tokio::test(start_paused = true)]
async fn a_refer_consult_lost_with_a_node_killed_unannounced_is_sent_again_at_the_verdict() {
    let mut c =
        cluster_telling("consult-answer-deadline-refer-read-dead", LONG_DURATION_SEC, true).await;
    let mut call = establish(&mut c).await;
    let refer_at = c.fh.now_ms();
    let mut refer = call
        .bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", "<sip:charlie@127.0.0.1:5090>")
        .send()
        .await;
    refer.expect(202).await;
    c.bob.receive_absorbing("NOTIFY", &["ACK"]).await.respond(200, "OK").await;
    let asked_at = consults_sent(&mut c, 1).await;
    let primary = call.primary.clone();
    c.fh.advance(Duration::from_millis(300)).await;
    c.fh.mark(&primary, None, "crash", "kill (unannounced)");
    worker(&mut c, &primary).crash();
    c.fh.advance(PROBE_READS_DEAD_AFTER).await;
    assert_eq!(c.asked.load(Ordering::SeqCst), 1, "nothing is sent again before the verdict");

    c.fh.mark(&primary, None, "health", "the proxy's probe reads it dead");
    c.proxy.set_health(&primary, WorkerHealth::Dead);
    let verdict_at = c.fh.now_ms();
    let resent_at = consults_sent(&mut c, 2).await;
    let lost_due = asked_at + (CONSULT_BUDGET + MARGIN).as_millis() as i64;
    assert!(resent_at < lost_due, "re-sent before the lost authorization is due");
    assert!(resent_at <= verdict_at + 500, "re-sent at the verdict");
    let backup = backup_of(&primary);
    assert!(worker(&mut c, backup).serves(&call.call_ref), "the backup took over");

    let mut notify = c.bob.receive_absorbing("NOTIFY", &["ACK"]).await;
    assert!(
        String::from_utf8_lossy(notify.request().body()).starts_with("SIP/2.0 500"),
        "the referrer is told the transfer failed"
    );
    notify.respond(200, "OK").await;
    assert_in(&c, window(refer_at, Duration::from_secs(REFER_EXPIRY_SEC as u64)), "told");
    assert_eq!(c.asked.load(Ordering::SeqCst), 2, "the authorization is sent again once");
    scenario_harness::callflow::hangup(&mut call.dialog, &c.bob).await;
    c.fh.mark(&primary, None, "depart", "the evicted pod is force-deleted");
    worker(&mut c, backup).simulate_peer_removed(&primary);
    assert_eq!(fully_over(&mut c, &call.call_ref).await, TerminationCause::RemoteBye);
}
