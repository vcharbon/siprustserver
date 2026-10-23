//! RFC 3261 §14.1 on the target side: the callee answered and was ACKed while
//! the caller's INVITE still has no final (a service withholds her answer). A
//! callee re-INVITE would reach the caller's early dialog while her own INVITE
//! is in progress there, so the B2BUA refuses it 491 (§14.2, the INVITE in
//! progress is not the sender's) and relays nothing. The service then answers
//! the caller, and her BYE ends the call.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, stated_by_response, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// On the callee's INVITE 2xx: confirm and ACK the callee and hold the
/// caller's answer for [`HOLD_SECS`]; then answer her under a fresh To-tag
/// with the callee's description.
mod withhold {
    use b2bua::rules::{
        Effect, Match, RelayedFinal, RuleAction, RuleCall, RuleContext, RuleDefinition,
        RuleHandleResult, ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, LegState, TimerKey, TimerType};
    use sip_message::Method;

    pub const HOLD_SECS: i64 = 2;

    define_service! {
        id: "withhold",
        machine: WITHHOLD,
        states: WithholdState { Waiting, Holding },
        init: |_call: &RuleCall| Some(ServiceSeed::new(WithholdState::Waiting.label())),
        rules: [ ack_callee_hold_caller(), answer_caller() ],
    }

    const ANSWER_TIMER: TimerType = TimerType::service(WITHHOLD, "answer");

    fn callee(ctx: &RuleContext) -> Option<String> {
        ctx.call.b_legs().first().map(|l| l.leg_id.clone())
    }

    fn ack_callee_hold_caller() -> RuleDefinition {
        sm_rule! {
            id: "withhold-ack-callee",
            machine: WITHHOLD,
            active: [ WithholdState::Waiting ],
            transitions: [ WithholdState::Waiting => WithholdState::Holding ],
            effects: [
                Effect::Originate { method: Method::Ack, label: "ACK → callee" },
                Effect::GuardTimer { timer: TimerType::NoAnswer, scope: call::TimerScopeKind::Leg, label: "disarm the ring timer" },
                Effect::GuardTimer { timer: ANSWER_TIMER, scope: call::TimerScopeKind::Call, label: "arm the held answer" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early]),
            handle: |ctx: &RuleContext| {
                let b = ctx.source_leg_id.to_string();
                Some(RuleHandleResult::new(vec![
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::AckLeg { leg_id: b.clone(), body: None },
                    RuleAction::cancel(TimerKey::leg(TimerType::NoAnswer, &b)),
                    RuleAction::arm(TimerKey::call(ANSWER_TIMER), TimerDelay::secs(HOLD_SECS)),
                    RuleAction::SetState { machine: WITHHOLD, to: WithholdState::Holding.label() },
                ]))
            },
        }
    }

    fn answer_caller() -> RuleDefinition {
        sm_rule! {
            id: "withhold-answer-caller",
            machine: WITHHOLD,
            active: [ WithholdState::Holding ],
            transitions: [ WithholdState::Holding => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 → caller under a fresh To-tag" },
                Effect::GuardTimer { timer: TimerType::SetupTimeout, scope: call::TimerScopeKind::Setup, label: "disarm the setup timer" },
                Effect::LifecycleCommand { label: "bridge caller and callee" },
            ],
            matcher: Match::timer().timer_type(ANSWER_TIMER),
            handle: |ctx: &RuleContext| {
                let b = callee(ctx)?;
                let a = ctx.call.a_leg().leg_id.clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::AnswerALegNewDialog {
                        status: 200,
                        reason: "OK".into(),
                        body: Some(b2bua_sdk::model::Body::from_leg(
                            super::ANSWER.as_bytes().to_vec(),
                            b.clone(),
                        )),
                        to_tag: None,
                        header_updates: vec![],
                        relayed: RelayedFinal::none(),
                    },
                    RuleAction::cancel(TimerKey::setup(TimerType::SetupTimeout)),
                    RuleAction::Merge { leg_a: a, leg_b: b },
                    RuleAction::ClearState { machine: WITHHOLD },
                ]))
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_toward_an_unanswered_caller_is_refused_491() {
    let h = Harness::new("b2bua-withheld-answer-glare");
    let alice = h.agent("alice", "127.0.0.1:6396").await;
    let bob = h.agent("bob", "127.0.0.1:6397").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 6397)))
            .services(vec![withhold::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:6398")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(ANSWER)).await;
    let refused = reinvite.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None, "a crossing 491 states none");

    h.advance(Duration::from_secs(withhold::HOLD_SECS as u64)).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let invites_to_alice = h
        .finish()
        .await
        .entries()
        .iter()
        .filter(|e| e.to == alice.addr() && e.raw.starts_with(b"INVITE "))
        .count();
    assert_eq!(invites_to_alice, 0, "the refused re-INVITE reached no caller");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}
