//! A received in-dialog request a service holds for a final it gives on a
//! later turn (ADR-0055 §2). The held request is a request record: the
//! service answers it with `AnswerHeld`; one it never answers is answered
//! `500` by the platform before its sender's Timer F (RFC 4320 §4.2), and one
//! still held when the call ends draws 487 (RFC 3261 §15.1.2).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The caller's INFO is held; the service's own timer answers it `202`, its
/// body stating what the service decided, `ANSWER_AFTER_SEC` later. A service
/// whose timer never fires leaves the hold to its deadline.
mod holdinfo {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, TimerKey, TimerType};

    pub const ANSWER_AFTER_SEC: i64 = 2;
    pub const DECIDED: &[u8] = b"decided";
    const ANSWER: TimerType = TimerType::service(HOLDINFO, "answer");

    define_service! {
        id: "holdinfo",
        machine: HOLDINFO,
        states: HiState { Waiting },
        init: |_call: &RuleCall| Some(ServiceSeed::new(HiState::Waiting.label())),
        rules: [ hold_info(), answer_held() ],
    }

    fn hold_info() -> RuleDefinition {
        sm_rule! {
            id: "holdinfo-hold",
            machine: HOLDINFO,
            active: [ HiState::Waiting ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: ANSWER, scope: call::TimerScopeKind::Call, label: "answer the held INFO" },
            ],
            matcher: Match::request().method("INFO").direction(Direction::FromA),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::HoldRequest,
                    RuleAction::arm(TimerKey::call(ANSWER), TimerDelay::secs(ANSWER_AFTER_SEC)),
                ]))
            },
        }
    }

    fn answer_held() -> RuleDefinition {
        sm_rule! {
            id: "holdinfo-answer",
            machine: HOLDINFO,
            active: [ HiState::Waiting ],
            transitions: [],
            effects: [ Effect::Respond { status: 202, label: "the held INFO's final" } ],
            matcher: Match::timer().timer_type(ANSWER),
            handle: |ctx: &RuleContext| {
                let caller = ctx.call.a_leg();
                let cseq = caller
                    .dialogs
                    .iter()
                    .flat_map(|d| d.requests.iter())
                    .find(|r| r.is_held())
                    .map(|r| r.inbound_cseq)?;
                Some(RuleHandleResult::new(vec![RuleAction::AnswerHeld {
                    leg_id: caller.leg_id.clone(),
                    cseq,
                    status: 202,
                    reason: "Accepted".into(),
                    body: DECIDED.to_vec(),
                    content_type: Some("text/plain".into()),
                }]))
            },
        }
    }
}

/// The caller's INFO is held and its service never answers it.
mod holdforever {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use call::Direction;

    fn hold(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::HoldRequest]))
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "holdforever-hold",
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::request().method("INFO").direction(Direction::FromA),
            handle: hold,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn service_def() -> b2bua::rules::ServiceDef {
        ServiceDef { id: "holdforever", init, rules }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// The service answers the held INFO on its timer's turn: the caller's INFO
/// draws that final, body and all, and nothing reaches the callee.
#[tokio::test(start_paused = true)]
async fn a_service_answers_a_held_request_on_a_later_turn() {
    let h = Harness::new("held-request-answered");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![holdinfo::service_def()])
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    h.advance(Duration::from_secs(1)).await;
    assert_eq!(alice.drain_tu().await, 0, "nothing answers the held INFO at once");
    h.advance(Duration::from_secs(2)).await;
    let answered = info.expect(202).await;
    assert_eq!(answered.body(), holdinfo::DECIDED, "the service's final carries its body");
    assert_eq!(bob.drain_tu().await, 0, "nothing reaches the callee");

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// A held INFO its service never answers is answered `500` by the platform at
/// its deadline, Timer F less T2 after it arrived: before the caller's own
/// Timer F, and never 408 (RFC 4320 §4.2). The call stays up.
#[tokio::test(start_paused = true)]
async fn the_platform_answers_a_held_request_no_service_answered_before_timer_f() {
    let h = Harness::new("held-request-deadline");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![holdforever::service_def()])
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    h.advance(Duration::from_secs(27)).await;
    alice.drain().await; // her INFO's retransmissions are absorbed, unanswered
    assert_eq!(alice.drain_tu().await, 0, "no final before the deadline (Timer F less T2 = 28 s)",);
    h.advance(Duration::from_millis(1_500)).await;
    info.expect(500).await;
    assert_eq!(b2bua.active_calls(), 1, "the call stays up");

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// A request still held when the call ends — the callee hangs up — draws
/// 487 (RFC 3261 §15.1.2) before the caller's dialog is BYEd.
#[tokio::test(start_paused = true)]
async fn a_request_still_held_when_the_call_ends_draws_487() {
    let h = Harness::new("held-request-call-ends");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![holdforever::service_def()])
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    h.advance(Duration::from_secs(1)).await;
    let mut bob_dialog = b_inv.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    info.expect(487).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}
