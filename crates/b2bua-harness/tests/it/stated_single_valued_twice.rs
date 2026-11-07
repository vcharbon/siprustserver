//! A service stating a single-valued header (RFC 3261 §7.3.1) on more than
//! one line, under any casing or compact form, never costs the message it
//! rides on: every line of that header is left out, the skip is a model defect
//! the run reports, and the message leaves (ADR-0053 §7). Each path a service
//! states lines on: a route's `update_headers`, an authored final
//! (`RespondToALeg`), and `DestroyLeg`'s CANCEL of a ringing callee and BYE of
//! an answered one.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{HeaderUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::header_update::SipHeaderUpdates;
use scenario_harness::Harness;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The model defect a skipped statement is counted under.
const DEFECT: &str = "stated-text-fits-its-position";

/// The `Reason` stated beside the repeated header, which rides.
const STATED: &str = "Q.850;cause=16";

/// A service's two statements of `Subject`: its long name, then its compact
/// form.
fn subject_twice() -> Vec<(String, String)> {
    vec![("Subject".to_string(), "one".to_string()), ("s".to_string(), "two".to_string())]
}

/// Ends the call when its timer fires: `DestroyLeg` the callee stating
/// `destroy`, answer a still-unanswered caller `480` stating `answer`, then the
/// graceful teardown of whatever is left.
macro_rules! releasing_service {
    ($module:ident, $id:literal, $rule:literal, $machine:ident, destroy: $destroy:expr, answer: $answer:expr) => {
        mod $module {
            use b2bua::rules::{
                Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition,
                RuleHandleResult, ServiceSeed, Terminal, TimerDelay,
            };
            use b2bua::{define_service, sm_rule};
            use call::{LegState, TimerKey, TimerType};
            use sip_message::Method;

            pub const FIRE_SEC: i64 = 5;

            define_service! {
                id: $id,
                machine: $machine,
                states: RlState { Armed },
                init: |_call: &RuleCall| {
                    Some(ServiceSeed::new(RlState::Armed.label()).with_actions(vec![
                        RuleAction::arm(
                            TimerKey::call(TimerType::service($machine, "release")),
                            TimerDelay::secs(FIRE_SEC),
                        ),
                    ]))
                },
                rules: [ release_on_fire() ],
            }

            fn release_on_fire() -> RuleDefinition {
                sm_rule! {
                    id: $rule,
                    machine: $machine,
                    active: [ RlState::Armed ],
                    transitions: [ RlState::Armed => Terminal ],
                    effects: [
                        Effect::Originate { method: Method::Bye, label: "CANCEL/BYE → callee" },
                        Effect::Respond { status: 480, label: "an unanswered caller's final" },
                        Effect::LifecycleCommand { label: "graceful teardown of the rest" },
                    ],
                    matcher: Match::timer().timer_type(TimerType::service($machine, "release")),
                    handle: |ctx: &RuleContext| {
                        let b = ctx.call.b_legs().first().expect("the callee leg").leg_id.to_string();
                        let mut actions = vec![RuleAction::DestroyLeg { leg_id: b, headers: $destroy }];
                        if ctx.call.a_leg().state() != LegState::Confirmed {
                            let answer: Vec<(String, String)> = $answer;
                            actions.push(RuleAction::RespondToALeg {
                                status: 480,
                                reason: "Temporarily Unavailable".into(),
                                header_updates: answer
                                    .into_iter()
                                    .map(|(name, value)| (name, Some(value)))
                                    .collect(),
                                contacts: vec![],
                            });
                        }
                        actions.push(RuleAction::BeginTermination {
                            reason: None,
                            cause: call::TerminationCause::Timeout(call::TimeoutKind::Setup),
                            by_leg: None,
                        });
                        actions.push(RuleAction::ClearState { machine: $machine });
                        Some(RuleHandleResult::new(actions))
                    },
                }
            }
        }
    };
}

releasing_service!(
    destroying,
    "subject-twice-destroy",
    "subject-twice-destroy-fire",
    SUBJECT_TWICE_DESTROY,
    destroy: {
        let mut stated = vec![("Reason".to_string(), super::STATED.to_string())];
        stated.extend(super::subject_twice());
        stated
    },
    answer: vec![]
);

releasing_service!(
    answering,
    "subject-twice-answer",
    "subject-twice-answer-fire",
    SUBJECT_TWICE_ANSWER,
    destroy: vec![("Reason".to_string(), super::STATED.to_string())],
    answer: super::subject_twice()
);

fn lines(msg: &sip_message::SipMessage, name: &str) -> Vec<String> {
    msg.raw(HeaderName::from(name)).map(str::to_string).collect()
}

fn subjects(req: &sip_message::SipRequest) -> usize {
    req.raw(HeaderName::Subject).count()
}

fn reasons(req: &sip_message::SipRequest) -> Vec<String> {
    req.raw(HeaderName::Reason).map(str::to_string).collect()
}

/// A route states `Subject` and `s`: the callee is dialled without either.
#[tokio::test(start_paused = true)]
async fn a_route_stating_a_single_valued_header_twice_dials_the_callee_without_it() {
    let mut s = B2buaScene::with_b2bua("subject-twice-route", |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                let mut headers = SipHeaderUpdates::new();
                headers.insert("Subject".into(), HeaderUpdate::Set(vec!["one".into()]));
                headers.insert("s".into(), HeaderUpdate::Set(vec!["two".into()]));
                headers.insert("X-Kept".into(), HeaderUpdate::Set(vec!["fine".into()]));
                route.update_headers = Some(headers);
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    let invite = sip_message::SipMessage::Request(uas.request().clone());
    assert!(lines(&invite, "Subject").is_empty(), "no Subject rides");
    assert_eq!(lines(&invite, "X-Kept"), ["fine"], "the other statement rides");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;

    let counted = s.b2bua.metrics().model_defects_total(DEFECT);
    let _defects = s.b2bua.take_model_violations();
    let _report = s.finish().await;
    assert_eq!(counted, 1, "the skip is counted");
}

/// A ringing callee, a service ending it and answering the caller: the
/// caller's 480 stating `Subject` and `s` reaches her without either.
#[tokio::test(start_paused = true)]
async fn an_authored_final_stating_a_single_valued_header_twice_reaches_the_caller_without_it() {
    let h = Harness::new("subject-twice-respond");
    let alice = h.agent("alice", "127.0.0.1:7600").await;
    let bob = h.agent("bob", "127.0.0.1:7601").await;
    let mut b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7601)))
            .services(vec![answering::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7602")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    h.advance(Duration::from_secs(answering::FIRE_SEC as u64 + 1)).await;
    let fin = call.expect(480).await;
    assert_eq!(fin.raw(HeaderName::Subject).count(), 0, "no Subject rides the 480");
    let mut cancel = bob.receive("CANCEL").await;
    assert_eq!(reasons(cancel.request()), [STATED]);
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.metrics().model_defects_total(DEFECT), 1, "the skip is counted");
    let defects = b2bua.take_model_violations();
    assert!(defects.iter().all(|d| d.contains("Subject is single-valued")), "{defects:?}");
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "the call's CDR");
    let _report = h.finish().await;
}

/// The callee rings: the service's CANCEL stating `Reason`, `Subject` and `s`
/// leaves with its `Reason` and no `Subject`.
#[tokio::test(start_paused = true)]
async fn a_destroyed_ringing_callee_is_cancelled_without_a_header_stated_twice() {
    let h = Harness::new("subject-twice-cancel");
    let alice = h.agent("alice", "127.0.0.1:7603").await;
    let bob = h.agent("bob", "127.0.0.1:7604").await;
    let mut b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7604)))
            .services(vec![destroying::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7605")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    h.advance(Duration::from_secs(destroying::FIRE_SEC as u64 + 1)).await;
    call.expect(480).await;
    let mut cancel = bob.receive("CANCEL").await;
    assert_eq!(subjects(cancel.request()), 0, "no Subject rides the CANCEL");
    assert_eq!(reasons(cancel.request()), [STATED], "the other statement rides");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.metrics().model_defects_total(DEFECT), 1, "the skip is counted");
    let defects = b2bua.take_model_violations();
    assert!(defects.iter().all(|d| d.contains("Subject is single-valued")), "{defects:?}");
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "the call's CDR");
    let _report = h.finish().await;
}

/// The callee answered: the service's BYE stating `Reason`, `Subject` and `s`
/// leaves with its `Reason` and no `Subject`, and the dialog is torn down.
#[tokio::test(start_paused = true)]
async fn a_destroyed_answered_callee_is_byed_without_a_header_stated_twice() {
    let h = Harness::new("subject-twice-bye");
    let alice = h.agent("alice", "127.0.0.1:7606").await;
    let bob = h.agent("bob", "127.0.0.1:7607").await;
    let mut b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7607)))
            .services(vec![destroying::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7608")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(destroying::FIRE_SEC as u64 + 1)).await;
    let mut bye_bob = bob.receive("BYE").await;
    assert_eq!(subjects(bye_bob.request()), 0, "no Subject rides the BYE");
    assert_eq!(reasons(bye_bob.request()), [STATED], "the other statement rides");
    bye_bob.respond(200, "OK").await;
    alice.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.metrics().model_defects_total(DEFECT), 1, "the skip is counted");
    let defects = b2bua.take_model_violations();
    assert!(defects.iter().all(|d| d.contains("Subject is single-valued")), "{defects:?}");
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "the call's CDR");
    let _report = h.finish().await;
}
