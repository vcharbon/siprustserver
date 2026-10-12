//! Each routing answer that dials opens an attempt, and the attempt ends
//! once: answered, failed, superseded by the next one, ended with the call, or
//! refused by admission before it dialled.
//!
//! - An initial route and the failover after its callee refused are two
//!   attempts, each naming the leg it dialled; the refused callee rang.
//! - A route the limiter refused — the initial one, or a failover one — is an
//!   attempt that ends `Refused` at once, and the services hear it start and
//!   end.
//! - A failover that opens while the failed leg is still being cancelled
//!   supersedes the earlier attempt: the timers the earlier attempt scoped
//!   leave the ledger with no rule cancelling them, and a service that did not
//!   claim the turn hears the attempt end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call::{AttemptCause, AttemptId, AttemptOutcome, Call, LimiterEntry};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::{Agent, ClientInvite, Harness};
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const FROM_TAG: &str = "alice-tag";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// A service that records every attempt it hears start and end under its
/// call-ext slice, and arms a long watchdog scoped to each attempt it hears
/// start — a watchdog it never cancels.
mod probe {
    use b2bua::rules::{
        Effect, LifecycleEvent, Match, RuleAction, RuleCall, RuleContext, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};

    pub const SLICE: &str = "attempt-probe";
    pub const WATCH: TimerType = TimerType::service(PROBE, "watch");

    define_service! {
        id: "attempt-probe",
        machine: PROBE,
        states: ProbeState { Listening },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ProbeState::Listening.label())),
        rules: [ on_started(), on_ended() ],
    }

    /// The slice with `entry` appended.
    fn append(ctx: &RuleContext, entry: String) -> RuleAction {
        let mut heard: Vec<serde_json::Value> = ctx
            .call
            .ext()
            .and_then(|e| e.get(SLICE))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        heard.push(entry.into());
        RuleAction::MergeCallExt { ext: [(SLICE.to_string(), heard.into())].into_iter().collect() }
    }

    fn on_started() -> b2bua::rules::RuleDefinition {
        sm_rule! {
            id: "attempt-probe-started",
            machine: PROBE,
            active: [ ProbeState::Listening ],
            transitions: [],
            effects: [
                Effect::GuardTimer {
                    timer: WATCH,
                    scope: call::TimerScopeKind::Attempt,
                    label: "watch the attempt",
                },
            ],
            matcher: Match::attempt_started(),
            handle: |ctx: &RuleContext| {
                let Some(LifecycleEvent::AttemptStarted { attempt, .. }) = ctx.lifecycle() else {
                    return None;
                };
                let cause = ctx.call.attempt(*attempt)?.cause;
                Some(RuleHandleResult::new(vec![
                    append(ctx, format!("started {} {cause:?}", attempt.0)),
                    RuleAction::arm(TimerKey::attempt(WATCH, *attempt), TimerDelay::secs(600)),
                ]))
            },
        }
    }

    fn on_ended() -> b2bua::rules::RuleDefinition {
        sm_rule! {
            id: "attempt-probe-ended",
            machine: PROBE,
            active: [ ProbeState::Listening ],
            transitions: [],
            effects: [],
            matcher: Match::attempt_ended(),
            handle: |ctx: &RuleContext| {
                let Some(LifecycleEvent::AttemptEnded { attempt, outcome }) = ctx.lifecycle() else {
                    return None;
                };
                Some(RuleHandleResult::new(vec![append(
                    ctx,
                    format!("ended {} {outcome:?}", attempt.0),
                )]))
            },
        }
    }
}

/// What the probe heard on `call`, in order.
fn heard(call: &Call) -> Vec<String> {
    call.ext
        .as_ref()
        .and_then(|e| e.get(probe::SLICE))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The attempts of `call`: cause, legs, outcome.
fn attempts(call: &Call) -> Vec<(AttemptCause, Vec<String>, Option<AttemptOutcome>)> {
    call.attempts.iter().map(|a| (a.cause, a.legs.clone(), a.outcome)).collect()
}

/// The probe's armed watchdogs, by the attempt each guards.
fn watched(call: &Call) -> Vec<AttemptId> {
    call.timers
        .iter()
        .filter(|t| *t.timer_type() == probe::WATCH)
        .filter_map(|t| t.key.attempt_id())
        .collect()
}

/// The terminated record of `call_ref`.
fn terminated(b2bua: &B2buaSut, call_ref: &str) -> Call {
    b2bua
        .terminated_calls()
        .into_iter()
        .find(|c| c.call_ref == call_ref)
        .expect("the call's terminated record is kept")
}

/// Carol answers `call`; alice hangs up; the call is reaped.
async fn carol_answers_then_hang_up(call: &mut ClientInvite, carol: &Agent, b2bua: &B2buaSut) {
    let mut uas = carol.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The route toward `bob` under a callback context, whose failure fails over
/// to `carol`.
fn failing_over(bob: u16, carol: u16, no_answer_sec: Option<i64>) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", bob);
                r.callback_context = Some("attempts".into());
                r.no_answer_ms = no_answer_sec.map(|s| s * 1000);
                NewCallResponse::Route(r)
            })
            .on_failure(move |_| CallTreatment::Route(route_to("127.0.0.1", carol)))
            .build(),
    )
}

/// Bob rings and refuses; the failover dials carol, who answers. Two
/// attempts: the route's, failed with bob's leg, which rang, and the
/// failover's, answered with carol's.
#[tokio::test(start_paused = true)]
async fn a_failover_opens_the_next_attempt_after_the_first_failed() {
    const CALL_ID: &str = "attempts-failover@127.0.0.1";
    let h = Harness::new("attempts-failover");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over(5070, 5071, None))
        .services(vec![probe::service_def()])
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    carol_answers_then_hang_up(&mut call, &carol, &b2bua).await;

    let record = terminated(&b2bua, &call_ref);
    assert_eq!(
        attempts(&record),
        vec![
            (AttemptCause::Route, vec!["b-1".to_string()], Some(AttemptOutcome::Failed)),
            (AttemptCause::Failover, vec!["b-2".to_string()], Some(AttemptOutcome::Answered)),
        ],
    );
    let ordinals: Vec<u32> = record.attempts.iter().map(|a| a.decision_ordinal).collect();
    assert_eq!(ordinals, vec![1, 2], "each under the decision that opened it");
    let legs: Vec<(Option<AttemptId>, bool)> =
        record.b_legs.iter().map(|l| (l.attempt, l.ring.rang())).collect();
    assert_eq!(legs, vec![(Some(AttemptId(0)), true), (Some(AttemptId(1)), false)]);
    assert_eq!(
        heard(&record),
        vec!["started 0 Route", "ended 0 Failed", "started 1 Failover", "ended 1 Answered"],
    );
    let _ = h.finish().await;
}

/// The ring deadline fires and bob's leg is CANCELled; bob withholds its 487
/// while the failover answer lands: the first attempt is superseded, its
/// watchdog leaves the ledger with no rule cancelling it, and the probe —
/// which claimed none of these turns — hears it end.
#[tokio::test(start_paused = true)]
async fn a_failover_while_the_leg_still_cancels_supersedes_the_attempt_and_its_timers() {
    const CALL_ID: &str = "attempts-superseded@127.0.0.1";
    let h = Harness::new("attempts-superseded");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over(5070, 5071, Some(2)))
        .services(vec![probe::service_def()])
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(watched(&live), vec![AttemptId(0)], "the first attempt is watched");

    h.advance(Duration::from_secs(3)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let mut carol_uas = carol.receive("INVITE").await;
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(
        live.attempts.iter().map(|a| a.outcome).collect::<Vec<_>>(),
        vec![Some(AttemptOutcome::Superseded), None],
        "the failover superseded the attempt whose leg still cancels",
    );
    assert_eq!(watched(&live), vec![AttemptId(1)], "the superseded attempt's watchdog is gone");

    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    assert_eq!(
        heard(&terminated(&b2bua, &call_ref)),
        vec!["started 0 Route", "ended 0 Superseded", "started 1 Failover", "ended 1 Answered"],
    );
    let _ = h.finish().await;
}

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// A real limiter on `net`, and a client of it.
async fn limiter(net: &SimulatedHttpNetwork) -> (Box<dyn HttpServerHandle>, Arc<dyn CallLimiter>) {
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    let client =
        Arc::new(HttpCallLimiter::new(Arc::new(net.clone()), laddr(), Duration::from_millis(150)));
    (handle, client)
}

/// A limiter that admits no call.
fn closed() -> Vec<LimiterEntry> {
    vec![LimiterEntry { id: "closed".into(), limit: 0 }]
}

/// Dial `call_id` toward the route the decision states; carol answers.
async fn refused_then_carol(
    name: &str,
    decision: Arc<dyn CallDecisionEngine>,
    refuse_at_bob: bool,
) -> Call {
    let call_id = format!("{name}@127.0.0.1");
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let http = SimulatedHttpNetwork::new();
    let (_server, client) = limiter(&http).await;
    let b2bua = B2buaSut::builder(decision)
        .services(vec![probe::service_def()])
        .limiter(client)
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", &call_id, FROM_TAG);
    let mut call = alice
        .invite(&bob)
        .identity(&call_id, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    if refuse_at_bob {
        bob.receive("INVITE").await.respond(486, "Busy Here").await;
        bob.receive("ACK").await;
    }
    carol_answers_then_hang_up(&mut call, &carol, &b2bua).await;
    let record = terminated(&b2bua, &call_ref);
    let _ = h.finish().await;
    record
}

/// The limiter refuses the initial route: an attempt that dialled nothing,
/// ended `Refused`; the limiter failover's route dials carol.
#[tokio::test(start_paused = true)]
async fn a_limiter_refused_initial_route_is_a_refused_attempt() {
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("attempts".into());
                r.call_limiter = closed();
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 5071)))
            .build(),
    );
    let record = refused_then_carol("attempts-refused-route", decision, false).await;
    assert_eq!(
        attempts(&record),
        vec![
            (AttemptCause::Route, vec![], Some(AttemptOutcome::Refused)),
            (AttemptCause::Failover, vec!["b-1".to_string()], Some(AttemptOutcome::Answered)),
        ],
    );
    assert_eq!(
        heard(&record),
        vec!["started 0 Route", "ended 0 Refused", "started 1 Failover", "ended 1 Answered"],
    );
}

/// Bob refuses; the failover's first route is refused by the limiter, its
/// re-consult dials carol: three attempts, the middle one `Refused`.
#[tokio::test(start_paused = true)]
async fn a_limiter_refused_failover_route_is_a_refused_attempt() {
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("attempts".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|req| {
                let mut r = route_to("127.0.0.1", 5071);
                if req.failure.origin != "call_limiter" {
                    r.call_limiter = closed();
                    r.callback_context = Some("attempts".into());
                }
                CallTreatment::Route(r)
            })
            .build(),
    );
    let record = refused_then_carol("attempts-refused-failover", decision, true).await;
    assert_eq!(
        attempts(&record),
        vec![
            (AttemptCause::Route, vec!["b-1".to_string()], Some(AttemptOutcome::Failed)),
            (AttemptCause::Failover, vec![], Some(AttemptOutcome::Refused)),
            (AttemptCause::Failover, vec!["b-2".to_string()], Some(AttemptOutcome::Answered)),
        ],
    );
    assert_eq!(
        heard(&record),
        vec![
            "started 0 Route",
            "ended 0 Failed",
            "started 1 Failover",
            "ended 1 Refused",
            "started 2 Failover",
            "ended 2 Answered",
        ],
    );
}

/// A decision engine whose `/call/failure` answers `delay` late.
struct SlowFailure {
    delay: Duration,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait::async_trait]
impl CallDecisionEngine for SlowFailure {
    async fn new_call(
        &self,
        req: b2bua::decision::NewCallRequest,
    ) -> Result<NewCallResponse, b2bua::decision::CallDecisionError> {
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: b2bua::decision::CallFailureRequest,
    ) -> Result<b2bua::decision::CallFailureResponse, b2bua::decision::CallDecisionError> {
        tokio::time::sleep(self.delay).await;
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: b2bua::decision::CallReferRequest,
    ) -> Result<b2bua::decision::CallReferResponse, b2bua::decision::CallDecisionError> {
        self.inner.call_refer(req).await
    }
    async fn call_release(
        &self,
        req: b2bua::decision::CallReleaseRequest,
    ) -> Result<b2bua::decision::CallReleaseResponse, b2bua::decision::CallDecisionError> {
        self.inner.call_release(req).await
    }
}

/// The ring deadline fires and the failure consult goes out; the caller
/// CANCELs while it is in flight, ending the route's attempt with the call.
/// The consult's fold then lands (its route refused by the limiter, its
/// re-consult answered by a reject, each 2 s late), listing the refused
/// route: the call is going away, so no attempt opens after it ended.
#[tokio::test(start_paused = true)]
async fn a_fold_landing_on_a_call_going_away_records_no_refused_route() {
    const CALL_ID: &str = "attempts-refused-going-away@127.0.0.1";
    let h = Harness::new("attempts-refused-going-away");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let (_server, client) = limiter(&http).await;
    let decision = Arc::new(SlowFailure {
        delay: Duration::from_secs(2),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.callback_context = Some("attempts".into());
                    r.no_answer_ms = Some(2_000);
                    NewCallResponse::Route(r)
                })
                .on_failure(|req| {
                    if req.failure.origin == "call_limiter" {
                        return CallTreatment::Reject(b2bua::decision::RejectDecision {
                            reject_code: 486,
                            reject_reason: Some("Busy Here".into()),
                            update_headers: None,
                            service_ext: Default::default(),
                            label: None,
                        });
                    }
                    let mut r = route_to("127.0.0.1", 5071);
                    r.call_limiter = closed();
                    r.callback_context = Some("attempts".into());
                    CallTreatment::Route(r)
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .services(vec![probe::service_def()])
        .limiter(client)
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_millis(2_100)).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;

    // The caller gives up while the consult is in flight; bob withholds his
    // 487 past the consult's answer.
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    h.advance(Duration::from_secs(6)).await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let record = terminated(&b2bua, &call_ref);
    assert_eq!(
        attempts(&record),
        vec![(AttemptCause::Route, vec!["b-1".to_string()], Some(AttemptOutcome::CallEnded))],
    );
    let _ = h.finish().await;
}

/// A service that, a second into the call, dials carol as an attempt of its
/// own beside the route's.
mod dialer {
    use b2bua::rules::{
        AttemptOpening, Effect, Match, RouteAnswer, RuleAction, RuleCall, RuleContext,
        RuleHandleResult, ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{AttemptCause, TimerKey, TimerType};
    use sip_message::Method;

    const DIAL: TimerType = TimerType::service(DIALER, "dial");

    define_service! {
        id: "attempt-dialer",
        machine: DIALER,
        states: DialerState { Waiting, Dialled },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(DialerState::Waiting.label()).with_actions(vec![
                RuleAction::arm(TimerKey::call(DIAL), TimerDelay::secs(1)),
            ]))
        },
        rules: [ dial() ],
    }

    fn dial() -> b2bua::rules::RuleDefinition {
        sm_rule! {
            id: "attempt-dialer-dial",
            machine: DIALER,
            active: [ DialerState::Waiting ],
            transitions: [ DialerState::Waiting => DialerState::Dialled ],
            effects: [
                Effect::Originate { method: Method::Invite, label: "INVITE → carol" },
            ],
            matcher: Match::timer().service_timers(DIALER),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::CreateLeg {
                        destination: ("127.0.0.1".into(), 5071),
                        new_ruri: None,
                        new_from: None,
                        new_to: None,
                        callback_context: None,
                        body_override: None,
                        header_updates: vec![],
                        header_adds: vec![],
                        kind: None,
                        attempt: Some(AttemptOpening::new(
                            AttemptCause::Transfer,
                            RouteAnswer::default(),
                        )),
                    },
                    RuleAction::SetState { machine: DIALER, to: DialerState::Dialled.label() },
                ]))
            },
        }
    }
}

/// An attempt opening while the current one's leg still rings, not being
/// cancelled, leaves it open beside the new one: its scoped watchdog stands.
#[tokio::test(start_paused = true)]
async fn an_attempt_whose_leg_still_rings_stays_open_beside_the_next() {
    const CALL_ID: &str = "attempts-beside@127.0.0.1";
    let h = Harness::new("attempts-beside");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![probe::service_def(), dialer::service_def()])
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut carol_uas = carol.receive("INVITE").await;
    // carol's 100 lets the CANCEL to come reach her (RFC 3261 §9.1).
    carol_uas.respond(100, "Trying").await;
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(
        live.attempts.iter().map(|a| a.outcome).collect::<Vec<_>>(),
        vec![None, None],
        "the route's attempt stays open beside the next",
    );
    assert_eq!(watched(&live), vec![AttemptId(0), AttemptId(1)], "both watchdogs stand");

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    carol.receive("CANCEL").await.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    carol_uas.respond(487, "Request Terminated").await;
    carol.receive("ACK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let record = terminated(&b2bua, &call_ref);
    assert_eq!(
        record.attempts.iter().map(|a| a.outcome).collect::<Vec<_>>(),
        vec![Some(AttemptOutcome::CallEnded), Some(AttemptOutcome::CallEnded)],
    );
    let _ = h.finish().await;
}

/// A failure fold naming a consult the call no longer awaits reaches no rule
/// and records none of the routes it lists as refused.
#[tokio::test(start_paused = true)]
async fn an_unawaited_fold_records_no_refused_route() {
    const CALL_ID: &str = "attempts-unawaited-refused@127.0.0.1";
    let h = Harness::new("attempts-unawaited-refused");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(failing_over(5070, 5071, None))
        .keep_terminated_calls()
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    b2bua.post_event(b2bua::CallEvent::InternalEvent {
        call_ref: call_ref.clone(),
        topic: "call-failure-result".into(),
        outcome: "reject".into(),
        payload: serde_json::json!({
            "code": 486,
            "failed_leg_id": "b-1",
            "consult_change": 999,
            "refused_routes": [{ "label": null, "service_ext": {}, "no_answer_ms": null }],
        }),
        body: Vec::new(),
        incarnation: None,
    });
    h.advance(Duration::from_millis(200)).await;
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(attempts(&live), vec![(AttemptCause::Route, vec!["b-1".to_string()], None)]);

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
