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
            effects: [ Effect::GuardTimer { timer: WATCH, label: "watch the attempt" } ],
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
                r.no_answer_timeout_sec = no_answer_sec;
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
        record.b_legs.iter().map(|l| (l.attempt, l.rang)).collect();
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
