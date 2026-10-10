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
use scenario_harness::{Harness, WaiverScope};
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
        states: RetargetState { Armed, Retargeted },
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
            transitions: [ RetargetState::Armed => RetargetState::Retargeted ],
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
                    RuleAction::SetState { machine: RETARGET, to: RetargetState::Retargeted.label() },
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

/// Two forks of the first leg ring; the first BYEs its early dialog while the
/// CANCEL is in flight, then the second's 200 crosses the CANCEL. The ACK of
/// that 200 names the second fork's dialog (RFC 3261 §13.2.2.4), not the
/// first's, which no 2xx ever established.
#[tokio::test(start_paused = true)]
async fn the_ack_of_a_crossing_200_names_its_own_fork() {
    let h = Harness::with_transit_delay("cancelled-fork-ack", 1);
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob1 answers 200 after taking the CANCEL (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob1"),
    );
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
    uas1.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    uas1.respond(180, "Ringing").with_to_tag("fork2").await;
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
    uas1.respond(200, "OK").with_to_tag("fork2").with_sdp(ANSWER).await;
    let ack = bob1.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("fork2"), "the ACK names the 200's fork");
    bob1.receive("BYE").await.respond(200, "OK").await;

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

/// A service that claims the first leg's 2xx while that leg reads Trying or
/// Early: it confirms the dialog and relays the answer to the caller.
mod claim_answer {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, LegState};

    define_service! {
        id: "claim-answer",
        machine: CLAIM,
        states: ClaimState { Watching },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ClaimState::Watching.label())),
        rules: [ claim_2xx() ],
    }

    fn claim_2xx() -> RuleDefinition {
        sm_rule! {
            id: "claim-answer-2xx",
            machine: CLAIM,
            active: [ ClaimState::Watching ],
            transitions: [],
            effects: [ Effect::Relay { label: "the answer → caller" } ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| ctx.source_leg_id == "b-1"),
            handle: |ctx: &RuleContext| {
                let b = ctx.source_leg_id.to_string();
                Some(RuleHandleResult::new(vec![
                    RuleAction::ConfirmDialog { leg_id: b },
                    RuleAction::RelayToPeer { transform: Default::default() },
                ]))
            },
        }
    }
}

/// The SUT with `services`, the first leg routed to `bob1`.
async fn sut(
    h: &Harness,
    bob1: &scenario_harness::Agent,
    services: Vec<b2bua::rules::ServiceDef>,
) -> B2buaSut {
    let first = bob1.addr();
    let decision =
        Arc::new(ScriptedDecisionEngine::route_all_to(first.ip().to_string(), first.port()));
    B2buaSut::builder(decision)
        .services(services)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(h, "b2bua", &h.next_addr().to_string())
        .await
}

/// A 2xx crossing the CANCEL of a retargeted fork is reaped (ACK + BYE,
/// RFC 3261 §9.1, §13.2.2.4): no service rule that claims a ringing leg's
/// answer claims a leg being cancelled, and the caller is answered by the
/// retarget alone.
#[tokio::test(start_paused = true)]
async fn a_2xx_crossing_a_retarget_cancel_is_reaped_whatever_service_claims_answers() {
    let h = Harness::with_transit_delay("cancelled-fork-crossing-2xx", 1);
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob1 answers 200 after taking the CANCEL (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob1"),
    );
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{RETARGET_PORT}")).await;
    let b2bua = sut(&h, &bob1, vec![retarget::service_def(), claim_answer::service_def()]).await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(retarget::AT_SEC as u64) + Duration::from_millis(300)).await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    let mut uas2 = bob2.receive("INVITE").await;

    uas1.respond(200, "OK").with_sdp(ANSWER).await;
    bob1.receive("ACK").await;
    bob1.receive("BYE").await.respond(200, "OK").await;
    h.advance(Duration::from_millis(100)).await;

    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.body(), ANSWER.as_bytes());
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

/// The retargeted fork's callee BYEs its early dialog while the CANCEL is in
/// flight; the call then ends. The record is released only once that fork's
/// INVITE has its 487 (RFC 3261 §9.1): its transaction still runs.
#[tokio::test(start_paused = true)]
async fn the_record_waits_for_the_487_of_a_fork_whose_callee_byed_its_early_dialog() {
    let h = Harness::with_transit_delay("cancelled-fork-release-waits", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{RETARGET_PORT}")).await;
    let b2bua = sut(&h, &bob1, vec![retarget::service_def()]).await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(retarget::AT_SEC as u64) + Duration::from_millis(300)).await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;

    h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "the callee's BYE on its early dialog is the deviation under test (RFC 3261 §15)",
    );
    let mut early = uas1.dialog();
    early.send_request(InDialogMethod::Bye).send().await.expect(200).await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.advance(Duration::from_secs(2)).await;
    assert!(!b2bua.is_reaped(), "the fork's INVITE still awaits its final");

    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A 2xx crossing the CANCEL of a fork that never rang is reaped; its turn
/// mints the caller-facing dialog of the stack's own tag, which no
/// provisional showed her. A request she sends under that tag names no
/// dialog she holds: 481 (RFC 3261 §12.2.2), and the call goes on.
#[tokio::test(start_paused = true)]
async fn a_caller_request_under_a_tag_she_was_never_shown_is_refused_481() {
    let h = Harness::with_transit_delay("cancelled-fork-unshown-face", 1);
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob1 answers 200 after taking the CANCEL (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob1"),
    );
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{RETARGET_PORT}")).await;
    let b2bua = sut(&h, &bob1, vec![retarget::service_def()]).await;
    let call_ref = call::derive_call_ref("w0", "unshown@alice", "alice-unshown");

    let mut call = alice
        .invite(&bob1)
        .identity("unshown@alice", "alice-unshown")
        // With no response to learn a target from, her in-dialog request
        // follows the Request-URI: it names the SUT.
        .ruri(format!("sip:bob1@{}", b2bua.addr))
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas1 = bob1.receive("INVITE").await;
    h.advance(Duration::from_secs(retarget::AT_SEC as u64) + Duration::from_millis(300)).await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    let mut uas2 = bob2.receive("INVITE").await;
    uas1.respond(200, "OK").with_sdp(ANSWER).await;
    bob1.receive("ACK").await;
    bob1.receive("BYE").await.respond(200, "OK").await;
    h.advance(Duration::from_millis(100)).await;

    let live = b2bua.live_call(&call_ref).expect("the call is live");
    let unshown = live
        .a_leg
        .dialogs
        .iter()
        .find(|d| !d.shown)
        .expect("the caller-facing dialog no provisional showed")
        .our_tag
        .clone();
    h.allow_violation(
        "mid-dialog-tags",
        "a request under a tag the caller was never shown is the deviation under test",
    );
    call.send_request(InDialogMethod::Info).with_to_tag(&unshown).send().await.expect(481).await;

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
