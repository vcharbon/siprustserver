//! A callee BYE on an early dialog this stack is CANCELling while the call
//! lives on (RFC 3261 §9.1, §15.1.2): a service retargets the call away from a
//! ringing fork — `CancelLeg` on it, a new leg dialled — and that fork's
//! callee BYEs its early dialog before its INVITE's final. The BYE crosses
//! the CANCEL: it is answered 200 and ends that fork alone; the INVITE's 487
//! still completes it, and the retarget's answer carries the call on. A
//! caller CANCEL arriving while that fork's CANCEL is in flight CANCELs only
//! the retarget: the fork is already being cancelled (§9.1).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob2 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Where the retarget dials.
const RETARGET_PORT: u16 = 5472;

/// The retarget service: on a deadline it CANCELs the first leg and dials the
/// retarget target, the call staying up.
mod retarget {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;
    use sip_message::Method;

    pub const AT_SEC: i64 = 5;
    const DEADLINE: TimerType = TimerType::service(RETARGET, "deadline");

    define_service! {
        id: "retarget",
        machine: RETARGET,
        states: RetargetState { Armed, Retargetd },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(RetargetState::Armed.label()).with_actions(vec![
                RuleAction::ScheduleTimer { timer_type: DEADLINE, delay: TimerDelay::secs(AT_SEC), leg_id: None },
            ]))
        },
        rules: [ retarget_on_deadline() ],
    }

    fn retarget_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "retarget-on-deadline",
            machine: RETARGET,
            active: [ RetargetState::Armed ],
            transitions: [ RetargetState::Armed => RetargetState::Retargetd ],
            effects: [
                Effect::Originate { method: Method::Cancel, label: "CANCEL → the first leg" },
                Effect::Originate { method: Method::Invite, label: "INVITE → the retarget target" },
            ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::CancelLeg { leg_id: "b-1".into() },
                    RuleAction::CreateLeg {
                        destination: ("127.0.0.1".into(), super::RETARGET_PORT),
                        new_ruri: None,
                        new_from: None,
                        new_to: None,
                        no_answer_timeout_sec: None,
                        callback_context: None,
                        body_override: None,
                        header_updates: vec![],
                        header_adds: vec![],
                        kind: None,
                    },
                    RuleAction::SetState { machine: RETARGET, to: RetargetState::Retargetd.label() },
                ]))
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_callee_bye_on_a_fork_being_cancelled_ends_that_fork_alone() {
    let h = Harness::with_transit_delay("cancelled-fork-bye", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{RETARGET_PORT}")).await;
    let first = bob1.addr();
    let decision =
        Arc::new(ScriptedDecisionEngine::route_all_to(first.ip().to_string(), first.port()));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![retarget::service_def()])
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(retarget::AT_SEC as u64) + Duration::from_millis(300)).await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    let mut uas2 = bob2.receive("INVITE").await;

    h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "the callee's BYE on its early dialog is the deviation under test (RFC 3261 §15)",
    );
    let mut early = uas1.dialog();
    early.send_request(InDialogMethod::Bye).send().await.expect(200).await;
    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await;
    h.advance(Duration::from_millis(100)).await;
    assert!(alice.try_receive_tolerating("BYE", &[]).await.is_none(), "the call lives on");
    assert_eq!(b2bua.active_calls(), 1, "the call lives on");

    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The caller CANCELs while the first fork's CANCEL awaits its 487: the
/// teardown CANCELs the retarget target and leaves the first fork to the CANCEL
/// already sent, which its 487 completes (RFC 3261 §9.1).
#[tokio::test(start_paused = true)]
async fn a_caller_cancel_leaves_a_fork_already_being_cancelled_to_its_cancel() {
    let h = Harness::with_transit_delay("cancelled-fork-caller-cancel", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{RETARGET_PORT}")).await;
    let first = bob1.addr();
    let decision =
        Arc::new(ScriptedDecisionEngine::route_all_to(first.ip().to_string(), first.port()));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![retarget::service_def()])
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(retarget::AT_SEC as u64) + Duration::from_millis(300)).await;
    let mut first_cancel = bob1.receive("CANCEL").await;
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    bob2.receive("CANCEL").await.respond(200, "OK").await;
    uas2.respond(487, "Request Terminated").await;
    bob2.receive("ACK").await;
    h.advance(Duration::from_millis(100)).await;
    assert!(
        bob1.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "the first fork is CANCELled once",
    );
    first_cancel.respond(200, "OK").await;
    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
