//! One setup budget per initial INVITE (ADR-0022 X1): from the INVITE's receipt
//! by the transaction layer, one worst-case bound over everything its turn
//! waits on before the final: the handler permit, the decision round trips and
//! the limiter's admits. When it runs out the caller gets the bare 503 of an
//! unavailable decision (RFC 3261 §17.1.1.2: past the 100 it sends nothing more
//! and only a final moves it on), no callee is dialled, and what an admit may
//! have counted is released.
//!
//! Every call here ends fully: a refused call with its CDR, an answered one
//! hung up by its caller.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::admission::Class;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallTreatment, NewCallRequest, NewCallResponse,
    ScriptedDecisionEngine,
};
use b2bua::limiter::LimiterEntry;
use b2bua::new_calls::Refusal;
use b2bua::turn_probe::{Hold, TurnMark, TurnPoint, TurnProbe};
use b2bua::TurnProbes;
use b2bua_harness::limiter::doubles::{
    admit_all, answer_nth_admit_late, fail_open, never_answer_admit, refuse_on, spy,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, Pace, RouterPace};
use call::{CdrEventType, TerminationCause};
use scenario_harness::callflow::{hangup, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, ClientInvite};
use sip_message::header::HeaderName;
use tokio::sync::watch;

/// `call_control_timeout_ms` at its default: the bound holds out of the box.
const BUDGET: Duration = Duration::from_millis(5_000);
/// One hop on the harness network.
const TRANSIT: Duration = Duration::from_millis(100);
/// How far either side of its due instant a final is read.
const MARGIN: Duration = Duration::from_millis(50);
/// When a final the turn sends at the budget reaches a caller who sent her
/// INVITE at 0: one transit to the receipt, the budget, one transit back.
const FINAL_AT: Duration = Duration::from_millis(100 + 5_000 + 100);

/// Holds every turn of the first call it sees at the turn's `Logged` point
/// while the gate is shut: a body keeping its permits past any budget, as a
/// body wedged on a call of its own does.
struct FirstCallHeld {
    first: OnceLock<String>,
    open: watch::Receiver<bool>,
}

impl TurnProbe for FirstCallHeld {
    fn reached(&self, mark: &TurnMark<'_>) -> Option<Hold> {
        if mark.point != TurnPoint::Logged {
            return None;
        }
        if self.first.get_or_init(|| mark.call_ref.to_string()) != mark.call_ref {
            return None;
        }
        let mut open = self.open.clone();
        Some(Box::pin(async move {
            let _ = open.wait_for(|open| *open).await;
        }))
    }
}

/// Answers every consult after its delay; records the From of each new call
/// asked and counts the failure consults.
struct Slow {
    inner: ScriptedDecisionEngine,
    new_call: Duration,
    failure: Duration,
    asked: Mutex<Vec<String>>,
    failures: AtomicUsize,
}

impl Slow {
    fn new(inner: ScriptedDecisionEngine, new_call: Duration, failure: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner,
            new_call,
            failure,
            asked: Mutex::new(Vec::new()),
            failures: AtomicUsize::new(0),
        })
    }

    /// Whether a new call from `who` asked its decision.
    fn asked_of(&self, who: &str) -> bool {
        self.asked.lock().unwrap().iter().any(|from| from.contains(who))
    }
}

#[async_trait]
impl CallDecisionEngine for Slow {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.asked.lock().unwrap().push(req.from.clone());
        tokio::time::sleep(self.new_call).await;
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.failure).await;
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// A route to bob holding limiter `cap`, with a failover context.
fn capped_route(bob_port: u16, cap: &str) -> b2bua::decision::RouteDecision {
    let mut route = route_to("127.0.0.1", bob_port);
    route.callback_context = Some("ctx".into());
    route.call_limiter = vec![LimiterEntry { id: cap.into(), limit: 10 }];
    route
}

/// An engine routing every call to bob under `cap`, and every failure consult
/// to bob under `failover_cap`.
fn capped_chain(bob_port: u16, cap: &str, failover_cap: &str) -> ScriptedDecisionEngine {
    let (first, next) = (capped_route(bob_port, cap), capped_route(bob_port, failover_cap));
    ScriptedDecisionEngine::builder()
        .fallback(move |_| NewCallResponse::Route(first.clone()))
        .on_failure(move |_| CallTreatment::Route(next.clone()))
        .build()
}

/// A b2bua of `concurrency` permits, a new-call share of half of them (at least
/// one), the first call's turns held behind `open` once they reach their write.
fn permits_held(
    open: watch::Receiver<bool>,
    decision: Arc<Slow>,
    concurrency: usize,
    budget_ms: i64,
) -> b2bua_harness::B2buaSutBuilder {
    let probe = TurnProbes::new(Arc::new(FirstCallHeld { first: OnceLock::new(), open }));
    B2buaSut::builder(decision).turn_probe(probe).tune(move |c| {
        c.event_dispatch_concurrency = concurrency;
        c.new_call_permit_share_percent = 50;
        c.call_control_timeout_ms = budget_ms;
    })
}

/// `call_control_timeout_ms`'s value here.
const BUDGET_MS: i64 = 5_000;

/// `caller`'s pending INVITE to `bob`, through the scene's b2bua.
async fn invite(s: &B2buaScene, caller: &Agent) -> ClientInvite {
    caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await
}

/// The callee answers, the caller ACKs and hangs up.
async fn answer_and_hang_up(call: &mut ClientInvite, bob: &Agent) {
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hangup(&mut dialog, bob).await;
}

/// The bare 503 of an unavailable decision.
fn assert_bare_503(resp: &sip_message::SipResponse) {
    assert_eq!(resp.reason(), "Service Unavailable");
    assert!(!resp.has(&HeaderName::Reason), "no Reason");
    assert!(!resp.has(&HeaderName::RetryAfter), "no Retry-After");
}

/// Nothing of the INVITE's final has reached `caller` by just before
/// [`FINAL_AT`], counted from `sent`; the 503 arrives just after it.
async fn final_503_at_the_budget(
    s: &B2buaScene,
    caller: &Agent,
    call: &mut ClientInvite,
    sent: tokio::time::Instant,
) {
    tokio::time::sleep_until(sent + FINAL_AT - MARGIN).await;
    assert!(caller.take_queued().await.is_none(), "no final before the budget");
    s.h.advance(MARGIN * 2).await;
    assert_bare_503(&call.expect(503).await);
}

/// The record of a call no decision reached.
fn assert_refused_cdr(s: &B2buaScene, call: &ClientInvite) {
    let cdrs = s.b2bua.cdr_records();
    let cdr = cdrs.iter().find(|r| r.a_leg.call_id == call.call_id()).expect("one CDR");
    assert!(cdr.b_legs.is_empty(), "no b-leg: {cdr:?}");
    assert!(cdr.decision_log.is_empty(), "no decision: {cdr:?}");
    assert_eq!(cdr.termination.as_ref().map(|t| t.cause), Some(TerminationCause::Admission));
    assert!(cdr.events.iter().any(|e| e.status_code == Some(503)), "its 503: {cdr:?}");
}

/// A turn that waited for its permit up to the budget runs without it: a 503,
/// no decision asked, no call dialled, and one CDR of a refused call.
#[tokio::test(start_paused = true)]
async fn a_turn_waiting_for_a_permit_past_the_budget_is_refused_and_asks_nothing() {
    let (open, gate) = watch::channel(false);
    let mut engine = None;
    let s = B2buaScene::with_b2bua("setup-budget-permit-wait", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        engine = Some(e.clone());
        permits_held(gate, e, 2, BUDGET_MS)
    })
    .await;
    let engine = engine.expect("the engine was built");

    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let sent = tokio::time::Instant::now();
    let mut waiting = invite(&s, &s.alice).await;
    waiting.expect(100).await;
    assert_eq!(s.b2bua.metrics().new_call_share_waits_total(), 1, "its turn waits for a permit");
    final_503_at_the_budget(&s, &s.alice, &mut waiting, sent).await;

    assert!(!engine.asked_of("alice"), "no decision asked");
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &waiting);

    open.send_replace(true);
    answer_and_hang_up(&mut holder, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;

    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 1, "the holder");
    assert_eq!(counts.rejected(Refusal::SetupBudget, Class::Normal), 1, "the refused turn");
    assert_eq!(counts.total(), 2, "one count per INVITE");
    let header = s.b2bua.overload().x_overload_header_value();
    assert!(header.ends_with("adm=1"), "the refused call is not counted as treated: {header}");

    let text = s.b2bua.metrics().prometheus_text();
    assert!(text.contains("b2bua_new_call_turn_wait_seconds_count 2"), "both turns: {text}");
    assert!(text.contains("b2bua_new_call_turn_wait_seconds_bucket{le=\"3\"} 1"), "{text}");
    assert!(text.contains("b2bua_new_call_turn_wait_seconds_bucket{le=\"5\"} 2"), "{text}");
    assert!(text.contains("b2bua_new_call_permit_hold_seconds_count 1"), "only the holder: {text}");
    let _ = s.finish().await;
}

/// The budget runs from the transaction layer's receipt, not from the router's:
/// an INVITE whose event waited behind a wedged router past the budget heard
/// its 100 from the layer, and is refused as soon as its turn runs, the
/// decision never asked.
#[tokio::test(start_paused = true)]
async fn a_turn_past_its_budget_behind_a_wedged_router_is_refused_unasked() {
    const NODE: &str = "w0";
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let mut engine = None;
    let s = B2buaScene::with_b2bua("setup-budget-wedged-router", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        engine = Some(e.clone());
        B2buaSut::builder(e).turn_probe(probe)
    })
    .await;
    let engine = engine.expect("the engine was built");

    pace.set(NODE, Pace::Frozen);
    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    s.h.advance(BUDGET).await;
    assert!(s.alice.take_queued().await.is_none(), "no final while the router is wedged");

    pace.set(NODE, Pace::Free);
    s.h.advance(TRANSIT * 2).await;
    assert_bare_503(&call.expect(503).await);
    assert!(!engine.asked_of("alice"), "no decision asked");
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::SetupBudget, Class::Normal), 1);
    assert_eq!(counts.total(), 1, "counted once");
    let _ = s.finish().await;
}

/// A copy of an INVITE waiting for its permit, on a new branch (RFC 3261
/// §8.2.2.2): its caller heard a 100 too, so its turn is due at its own budget and
/// answers its 482 then, whatever still holds the permits. It is no new call.
#[tokio::test(start_paused = true)]
async fn a_copy_waiting_for_a_permit_is_answered_at_its_own_budget() {
    let (open, gate) = watch::channel(false);
    let s = B2buaScene::with_b2bua("setup-budget-copy", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        permits_held(gate, Slow::new(inner, Duration::ZERO, Duration::ZERO), 2, BUDGET_MS)
    })
    .await;
    s.h.waive(
        scenario_harness::WaiverScope::rule("cseq-in-dialog-order", "the copy under test")
            .on_party("alice"),
    );
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let mut first = s
        .alice
        .invite(&s.bob)
        .identity("copy@127.0.0.1", "copy-tag")
        .cseq(1)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    first.expect(100).await;
    let sent = tokio::time::Instant::now();
    let mut copy = s
        .alice
        .invite(&s.bob)
        .identity("copy@127.0.0.1", "copy-tag")
        .cseq(1)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    copy.expect(100).await;
    assert_bare_503(&first.expect(503).await);

    tokio::time::sleep_until(sent + FINAL_AT - MARGIN).await;
    assert!(s.alice.take_queued().await.is_none(), "no final before its own budget");
    s.h.advance(MARGIN * 2).await;
    copy.expect(482).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &first);
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::SetupBudget, Class::Normal), 1, "the first alone");
    assert_eq!(counts.refused_copies(), 1, "a copy is no new call");

    open.send_replace(true);
    answer_and_hang_up(&mut holder, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _ = s.finish().await;
}

/// A CANCEL that crosses the 503 of a spent turn: it reaches the node just after
/// the final left, and the caller's INVITE keeps that one final (RFC 3261 §9.2).
#[tokio::test(start_paused = true)]
async fn a_cancel_crossing_the_budgets_503_draws_no_second_final() {
    let (open, gate) = watch::channel(false);
    let s = B2buaScene::with_b2bua("setup-budget-cancel-crossing", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        permits_held(gate, Slow::new(inner, Duration::ZERO, Duration::ZERO), 2, BUDGET_MS)
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let sent = tokio::time::Instant::now();
    let mut waiting = invite(&s, &s.alice).await;
    waiting.expect(100).await;
    // Sent while the 503 is on its way: each reaches the far end after the other left.
    tokio::time::sleep_until(sent + FINAL_AT - MARGIN).await;
    let mut cancel = waiting.cancel().await;
    assert_bare_503(&waiting.expect(503).await);
    cancel.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &waiting);
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::SetupBudget, Class::Normal), 1, "refused, not cancelled");
    assert_eq!(counts.cancelled(Class::Normal), 0);
    assert!(s.alice.take_queued().await.is_none(), "the 503 was its one final");

    open.send_replace(true);
    answer_and_hang_up(&mut holder, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _ = s.finish().await;
}

/// A route answered at the very instant the budget runs out is applied with none
/// left: no admit leaves for it, so nothing can have been counted.
#[tokio::test(start_paused = true)]
async fn a_route_answered_at_the_budget_sends_no_admit() {
    let limiter = spy(admit_all());
    let spied = limiter.clone();
    let s = B2buaScene::with_b2bua("setup-budget-no-admit", |bob_port| {
        let inner = capped_chain(bob_port, "trunk", "trunk");
        B2buaSut::builder(Slow::new(inner, BUDGET, Duration::ZERO)).limiter(spied)
    })
    .await;

    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    s.h.advance(BUDGET).await;
    assert_bare_503(&call.expect(503).await);

    assert!(limiter.admitted_keys().is_empty(), "no admit sent: {:?}", limiter.admitted_keys());
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    assert!(limiter.released_keys().is_empty(), "nothing to release");
    let _ = s.finish().await;
}

/// Slow answers and limiter refusals: the chain is cut where the budget runs
/// out, in the third failure consult, well before its depth limit would have
/// ended it. The caller gets the 503 at the budget, nothing is dialled, and
/// the key the admits were sent under is released.
#[tokio::test(start_paused = true)]
async fn a_chain_of_slow_answers_and_refusals_is_cut_at_the_budget() {
    let limiter = spy(refuse_on("cap", fail_open()));
    let mut engine = None;
    let spied = limiter.clone();
    let s = B2buaScene::with_b2bua("setup-budget-chain-cut", |bob_port| {
        let inner = capped_chain(bob_port, "cap", "cap");
        let e = Slow::new(inner, Duration::from_millis(1_000), Duration::from_millis(1_500));
        engine = Some(e.clone());
        B2buaSut::builder(e).limiter(spied)
    })
    .await;
    let engine = engine.expect("the engine was built");

    let sent = tokio::time::Instant::now();
    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    final_503_at_the_budget(&s, &s.alice, &mut call, sent).await;

    assert_eq!(engine.failures.load(Ordering::SeqCst), 3, "the third consult is cut");
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    let admitted = limiter.admitted_keys();
    assert_eq!(admitted.len(), 3, "the route's admit and two failovers': {admitted:?}");
    settle_until(|| !limiter.released_keys().is_empty()).await;
    assert_eq!(limiter.released_keys(), vec![admitted[0].clone()], "the key released once");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 1, "its turn ran: an accepted call's outcome");
    assert_eq!(counts.rejected_sum(), 0);
    let _ = s.finish().await;
}

/// An admit still unanswered at the budget is cut there, short of its own
/// bound: the callee is never dialled and the admit, which may have landed, has
/// its key released.
#[tokio::test(start_paused = true)]
async fn an_admit_unanswered_at_the_budget_is_cut_and_its_key_released() {
    let limiter = spy(never_answer_admit(Duration::from_secs(4), fail_open()));
    let spied = limiter.clone();
    let s = B2buaScene::with_b2bua("setup-budget-admit-cut", |bob_port| {
        let inner = capped_chain(bob_port, "trunk", "trunk");
        let e = Slow::new(inner, Duration::from_millis(3_000), Duration::ZERO);
        B2buaSut::builder(e).limiter(spied)
    })
    .await;

    let sent = tokio::time::Instant::now();
    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    final_503_at_the_budget(&s, &s.alice, &mut call, sent).await;

    // Past the admit's own bound, which ended it open on an unbudgeted turn.
    s.h.advance(Duration::from_secs(4)).await;
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    let admitted = limiter.admitted_keys();
    assert_eq!(admitted.len(), 1, "one admit sent");
    settle_until(|| !limiter.released_keys().is_empty()).await;
    assert_eq!(limiter.released_keys(), admitted, "its key released");
    let _ = s.finish().await;
}

/// An admit whose grant comes back at the very instant the budget runs out is a
/// route past its budget all the same: the callee is not dialled, and the hold
/// the limiter granted is released.
#[tokio::test(start_paused = true)]
async fn a_hold_granted_at_the_budget_dials_nothing_and_is_released() {
    let answered_late = answer_nth_admit_late(1, Duration::from_millis(1_000), admit_all());
    let limiter = spy(answered_late);
    let spied = limiter.clone();
    let s = B2buaScene::with_b2bua("setup-budget-granted-at-the-budget", |bob_port| {
        let inner = capped_chain(bob_port, "trunk", "trunk");
        let e = Slow::new(inner, Duration::from_millis(4_000), Duration::ZERO);
        B2buaSut::builder(e).limiter(spied)
    })
    .await;

    let sent = tokio::time::Instant::now();
    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    final_503_at_the_budget(&s, &s.alice, &mut call, sent).await;

    s.h.advance(Duration::from_secs(1)).await;
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    let admitted = limiter.admitted_keys();
    assert_eq!(admitted.len(), 1, "one admit sent");
    settle_until(|| !limiter.released_keys().is_empty()).await;
    assert_eq!(limiter.released_keys(), admitted, "the hold is released");
    let _ = s.finish().await;
}

/// A chain that ends inside the budget reaches its route, whatever each step
/// took: a refusal, a failover refused again, a failover admitted.
#[tokio::test(start_paused = true)]
async fn a_healthy_chain_inside_the_budget_reaches_its_route() {
    let limiter = spy(refuse_on("cap", fail_open()));
    let spied = limiter.clone();
    let s = B2buaScene::with_b2bua("setup-budget-healthy-chain", |bob_port| {
        let first = capped_route(bob_port, "cap");
        let again = capped_route(bob_port, "cap");
        let open = capped_route(bob_port, "open");
        let consults = AtomicUsize::new(0);
        let inner = ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(first.clone()))
            .on_failure(move |_| match consults.fetch_add(1, Ordering::SeqCst) {
                0 => CallTreatment::Route(again.clone()),
                _ => CallTreatment::Route(open.clone()),
            })
            .build();
        let step = Duration::from_millis(1_400);
        B2buaSut::builder(Slow::new(inner, step, step)).limiter(spied)
    })
    .await;

    let mut call = invite(&s, &s.alice).await;
    call.expect(100).await;
    s.h.advance(Duration::from_millis(4_200)).await;
    answer_and_hang_up(&mut call, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(limiter.admitted_keys().len(), 3, "the route and its two failovers");
    let _ = s.finish().await;
}

/// A plan that keeps returning limited routes ends at the chain's depth with the
/// bare 503 of an unavailable decision, inside the budget: nothing is dialled,
/// the key the admits were sent under is released, and the call writes its CDR
/// with no decision on it.
#[tokio::test(start_paused = true)]
async fn a_chain_past_its_depth_answers_the_bare_503() {
    let limiter = spy(refuse_on("cap", fail_open()));
    let spied = limiter.clone();
    let mut engine = None;
    let s = B2buaScene::with_b2bua("setup-budget-chain-depth", |bob_port| {
        let inner = capped_chain(bob_port, "cap", "cap");
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        engine = Some(e.clone());
        B2buaSut::builder(e).limiter(spied)
    })
    .await;
    let engine = engine.expect("the engine was built");

    let mut call = invite(&s, &s.alice).await;
    assert_bare_503(&call.expect(503).await);

    assert_eq!(engine.failures.load(Ordering::SeqCst), 5, "the chain ran to its depth");
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_refused_cdr(&s, &call);
    let admitted = limiter.admitted_keys();
    assert_eq!(admitted.len(), 6, "the route's admit and five failovers': {admitted:?}");
    settle_until(|| !limiter.released_keys().is_empty()).await;
    assert_eq!(limiter.released_keys(), vec![admitted[0].clone()], "the key released once");
    let _ = s.finish().await;
}

/// An emergency INVITE draws the shared pool alone and holds no privilege over
/// the budget: a caller left waiting serves nobody.
#[tokio::test(start_paused = true)]
async fn an_emergency_invite_is_held_to_the_same_budget() {
    let (open, gate) = watch::channel(false);
    let mut engine = None;
    let s = B2buaScene::with_b2bua("setup-budget-emergency", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        engine = Some(e.clone());
        permits_held(gate, e, 1, BUDGET_MS)
    })
    .await;
    let engine = engine.expect("the engine was built");

    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let sent = tokio::time::Instant::now();
    let mut waiting = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header("Resource-Priority", "esnet.0")
        .through(s.b2bua.addr)
        .send()
        .await;
    waiting.expect(100).await;
    final_503_at_the_budget(&s, &s.alice, &mut waiting, sent).await;
    assert!(!engine.asked_of("alice"), "no decision asked");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::SetupBudget, Class::Emergency), 1, "in its own class");

    open.send_replace(true);
    answer_and_hang_up(&mut holder, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _ = s.finish().await;
}

/// A setup its caller CANCELed while its turn waited has its 487 already: at
/// the budget its turn runs and ends the setup as cancelled, not refused, with
/// the permit still held elsewhere and no second final.
#[tokio::test(start_paused = true)]
async fn a_setup_canceled_while_waiting_ends_at_the_budget_as_cancelled() {
    let (open, gate) = watch::channel(false);
    let mut engine = None;
    let s = B2buaScene::with_b2bua("setup-budget-canceled", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        engine = Some(e.clone());
        permits_held(gate, e, 2, BUDGET_MS)
    })
    .await;
    let engine = engine.expect("the engine was built");

    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let mut waiting = invite(&s, &s.alice).await;
    waiting.expect(100).await;
    let mut cancel = waiting.cancel().await;
    cancel.expect(200).await;
    waiting.expect(487).await;

    s.h.advance(BUDGET).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let cdrs = s.b2bua.cdr_records();
    let cdr = &cdrs[0];
    assert_eq!(cdr.a_leg.call_id, waiting.call_id());
    assert_eq!(cdr.termination.as_ref().map(|t| t.cause), Some(TerminationCause::RemoteCancel));
    let events: Vec<_> = cdr.events.iter().map(|e| e.event_type).collect();
    assert_eq!(events, [CdrEventType::InviteReceived, CdrEventType::Cancel], "{cdr:?}");
    assert!(!engine.asked_of("alice"), "no decision asked");
    assert!(s.alice.take_queued().await.is_none(), "the 487 was its one final");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.cancelled(Class::Normal), 1, "counted as cancelled");
    assert_eq!(counts.rejected_sum(), 0, "not as refused");

    open.send_replace(true);
    answer_and_hang_up(&mut holder, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _ = s.finish().await;
}

/// `call_control_timeout_ms <= 0` is the escape hatch: no budget, a turn waits
/// for its permit as long as it takes, then runs as any other.
#[tokio::test(start_paused = true)]
async fn with_the_budget_off_a_turn_waits_for_its_permit_as_long_as_it_takes() {
    let (open, gate) = watch::channel(false);
    let s = B2buaScene::with_b2bua("setup-budget-off", |bob_port| {
        let inner = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        let e = Slow::new(inner, Duration::ZERO, Duration::ZERO);
        permits_held(gate, e, 2, 0)
    })
    .await;

    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holder = invite(&s, &carol).await;
    s.h.advance(TRANSIT * 2).await;

    let mut waiting = invite(&s, &s.alice).await;
    waiting.expect(100).await;
    s.h.advance(BUDGET * 4).await;
    assert!(s.alice.take_queued().await.is_none(), "no final: the turn still waits");
    assert_eq!(s.b2bua.cdr_records().len(), 0, "nothing refused");

    // The holder's INVITE leaves, then the permit it frees runs the other's.
    open.send_replace(true);
    let mut first = s.bob.receive("INVITE").await;
    let mut second = s.bob.receive("INVITE").await;
    for (uas, call) in [(&mut first, &mut holder), (&mut second, &mut waiting)] {
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        s.bob.receive("ACK").await;
        hangup(&mut dialog, &s.bob).await;
    }
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _ = s.finish().await;
}
