//! A service that ends the caller's answered leg with `DestroyLeg` sends the
//! caller its `BYE` (RFC 3261 §15): the a-leg dialog ends on the wire, never
//! by the stack's record alone.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Ends the call when its timer fires: `DestroyLeg` the caller, then the
/// callee, then the graceful teardown.
mod caller_releaser {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};
    use sip_message::Method;

    pub const FIRE_SEC: i64 = 5;

    define_service! {
        id: "caller-releaser",
        machine: CALLER_RELEASER,
        states: CrState { Armed },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(CrState::Armed.label()).with_actions(vec![
                RuleAction::arm(
                    TimerKey::call(TimerType::service(CALLER_RELEASER, "release")),
                    TimerDelay::secs(FIRE_SEC),
                ),
            ]))
        },
        rules: [ release_on_fire() ],
    }

    fn release_on_fire() -> RuleDefinition {
        sm_rule! {
            id: "caller-releaser-fire",
            machine: CALLER_RELEASER,
            active: [ CrState::Armed ],
            transitions: [ CrState::Armed => Terminal ],
            effects: [
                Effect::Originate { method: Method::Bye, label: "BYE → caller, then BYE → callee" },
                Effect::LifecycleCommand { label: "graceful teardown of the rest" },
            ],
            matcher: Match::timer().timer_type(TimerType::service(CALLER_RELEASER, "release")),
            handle: |ctx: &RuleContext| {
                let a = ctx.call.a_leg().leg_id.to_string();
                let b = ctx.call.b_legs().first().expect("the callee leg").leg_id.to_string();
                Some(RuleHandleResult::new(vec![
                    RuleAction::DestroyLeg { leg_id: a, headers: vec![] },
                    RuleAction::DestroyLeg { leg_id: b, headers: vec![] },
                    RuleAction::BeginTermination {
                        reason: None,
                        cause: call::TerminationCause::Timeout(call::TimeoutKind::Setup),
                        by_leg: None,
                    },
                    RuleAction::ClearState { machine: CALLER_RELEASER },
                ]))
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn destroying_the_answered_caller_leg_sends_her_a_bye() {
    let h = Harness::new("destroy-leg-caller-bye");
    let alice = h.agent("alice", "127.0.0.1:7460").await;
    let bob = h.agent("bob", "127.0.0.1:7461").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7461)))
            .services(vec![caller_releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7462")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(caller_releaser::FIRE_SEC as u64 + 1)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
