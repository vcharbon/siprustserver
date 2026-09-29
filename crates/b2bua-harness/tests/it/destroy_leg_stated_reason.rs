//! A service that ends a leg on its own states the release's `Reason`
//! (RFC 3326) on the request `DestroyLeg` mints for it: the `CANCEL` of a
//! ringing callee (§2 names `CANCEL`), the `BYE` of an answered one. A
//! teardown stating nothing carries no `Reason`, whatever the peer sent.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The `Reason` the service states on its release.
const STATED: &str = "Q.850;cause=16";

/// Ends the call when its timer fires: `DestroyLeg` the callee stating
/// [`STATED`], answer a still-unanswered caller `480`, then the graceful
/// teardown of whatever is left.
mod releaser {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{LegState, TimerType};
    use sip_message::Method;

    pub const FIRE_SEC: i64 = 5;

    define_service! {
        id: "releaser",
        machine: RELEASER,
        states: RlState { Armed },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(RlState::Armed.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(RELEASER, "release"),
                    delay: TimerDelay::secs(FIRE_SEC),
                    leg_id: None,
                },
            ]))
        },
        rules: [ release_on_fire() ],
    }

    fn release_on_fire() -> RuleDefinition {
        sm_rule! {
            id: "releaser-fire",
            machine: RELEASER,
            active: [ RlState::Armed ],
            transitions: [ RlState::Armed => Terminal ],
            effects: [
                Effect::Originate { method: Method::Bye, label: "CANCEL/BYE → callee, stating the Reason" },
                Effect::Respond { status: 480, label: "an unanswered caller's final" },
                Effect::LifecycleCommand { label: "graceful teardown of the rest" },
            ],
            matcher: Match::timer().timer_type(TimerType::service(RELEASER, "release")),
            handle: |ctx: &RuleContext| {
                let b = ctx.call.b_legs().first().expect("the callee leg").leg_id.to_string();
                let mut actions = vec![RuleAction::DestroyLeg {
                    leg_id: b,
                    headers: vec![("Reason".to_string(), super::STATED.to_string())],
                }];
                if ctx.call.a_leg().state != LegState::Confirmed {
                    actions.push(RuleAction::RespondToALeg {
                        status: 480,
                        reason: "Temporarily Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    });
                }
                actions.push(RuleAction::BeginTermination {
                    reason: None,
                    cause: call::TerminationCause::Timeout(call::TimeoutKind::Setup),
                    by_leg: None,
                });
                actions.push(RuleAction::ClearState { machine: RELEASER });
                Some(RuleHandleResult::new(actions))
            },
        }
    }
}

fn reasons(req: &sip_message::SipRequest) -> Vec<String> {
    req.raw(HeaderName::Reason).map(str::to_string).collect()
}

/// The callee rings: the service's `CANCEL` carries the stated `Reason`.
#[tokio::test(start_paused = true)]
async fn the_cancel_of_a_ringing_callee_carries_the_stated_reason() {
    let h = Harness::new("destroy-leg-stated-reason-cancel");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)))
            .services(vec![releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    h.advance(Duration::from_secs(releaser::FIRE_SEC as u64 + 1)).await;
    call.expect(480).await;
    let mut cancel = bob.receive("CANCEL").await;
    assert_eq!(reasons(cancel.request()), [STATED], "the CANCEL states the release's Reason");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The callee answered: the service's `BYE` carries the stated `Reason`, the
/// caller's `BYE` of the same teardown states none.
#[tokio::test(start_paused = true)]
async fn the_bye_of_an_answered_callee_carries_the_stated_reason() {
    let h = Harness::new("destroy-leg-stated-reason-bye");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)))
            .services(vec![releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:5081")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(releaser::FIRE_SEC as u64 + 1)).await;
    let mut bye_bob = bob.receive("BYE").await;
    assert_eq!(reasons(bye_bob.request()), [STATED], "the BYE states the release's Reason");
    bye_bob.respond(200, "OK").await;
    let mut bye_alice = alice.receive("BYE").await;
    assert!(reasons(bye_alice.request()).is_empty(), "nothing stated for the caller's BYE");
    bye_alice.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
