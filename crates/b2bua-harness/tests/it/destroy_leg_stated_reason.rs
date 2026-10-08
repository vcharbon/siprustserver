//! A service that ends a leg on its own states the release's `Reason`
//! (RFC 3326) on the request `DestroyLeg` mints for it: the `CANCEL` of a
//! ringing callee (§2 names `CANCEL`), the `BYE` of an answered one. A
//! `DestroyLeg` stating nothing carries no `Reason`, even when the caller's
//! `BYE` that prompted it stated one.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
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
    use call::{ByeDisposition, Direction, LegState, TimerType};
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
        rules: [ release_on_fire(), end_callee_on_caller_bye() ],
    }

    /// The caller's `BYE`: answer it and `DestroyLeg` the callee stating
    /// nothing, in place of the core relay.
    fn end_callee_on_caller_bye() -> RuleDefinition {
        sm_rule! {
            id: "releaser-caller-bye",
            machine: RELEASER,
            active: [ RlState::Armed ],
            transitions: [ RlState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 OK [bye] → caller" },
                Effect::Originate { method: Method::Bye, label: "BYE → callee, stating nothing" },
                Effect::LifecycleCommand { label: "graceful teardown of the rest" },
            ],
            matcher: Match::request().method("BYE").direction(Direction::FromA),
            handle: |ctx: &RuleContext| {
                let a = ctx.source_leg_id.to_string();
                let b = ctx.call.b_legs().first().expect("the callee leg").leg_id.to_string();
                Some(RuleHandleResult::new(vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::TerminateLeg {
                        leg_id: a.clone(),
                        bye_disposition: Some(ByeDisposition::ByeReceived),
                    },
                    RuleAction::DestroyLeg { leg_id: b, headers: vec![] },
                    RuleAction::BeginTermination {
                        reason: None,
                        cause: call::TerminationCause::RemoteBye,
                        by_leg: Some(a),
                    },
                    RuleAction::ClearState { machine: RELEASER },
                ]))
            },
        }
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

/// The caller's `BYE` states a `Reason`; the service's `DestroyLeg` of the
/// callee states nothing, so the callee's `BYE` carries no `Reason`.
#[tokio::test(start_paused = true)]
async fn a_bye_stating_nothing_carries_no_reason_whatever_the_caller_stated() {
    let h = Harness::new("destroy-leg-stated-reason-none");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5072)))
            .services(vec![releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:5082")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog
        .send_request(InDialogMethod::Bye)
        .with_header("Reason", "Q.850;cause=31")
        .send()
        .await;
    bye.expect(200).await;
    let mut bye_bob = bob.receive("BYE").await;
    assert!(reasons(bye_bob.request()).is_empty(), "nothing stated, nothing relayed");
    bye_bob.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A service ends an answered callee whose 2xx carried no new offer: the
/// callee's reliable 183 carried it on a delayed-offer INVITE, the caller
/// never PRACKed it and has not ACKed yet. The callee sees the PRACK still
/// owed, answering the offer rejecting every stream (RFC 3262 §4-§5), then
/// this stack's bare ACK of his 2xx (RFC 3261 §13.2.2.4), then the `BYE`.
#[tokio::test(start_paused = true)]
async fn a_destroyed_callee_is_pracked_for_an_offer_still_owed() {
    let h = Harness::new("destroy-leg-owed-prack");
    for (rule, party, why) in [
        ("unacked-reliable-provisional", "alice", "alice never PRACKs the 183 shown to her"),
        ("no-ack-to-dialog-creating-2xx", "alice", "alice has not ACKed when the service fires"),
        (
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "bob",
            "bob answers before his offer is PRACKed",
        ),
        (
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "b2bua",
            "the SUT relays bob's early 200 toward alice",
        ),
    ] {
        h.waive(scenario_harness::WaiverScope::rule(rule, why).on_party(party));
    }
    let alice = h.agent("alice", "127.0.0.1:7450").await;
    let bob = h.agent("bob", "127.0.0.1:7451").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7451)))
            .services(vec![releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7452")
            .await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "31")
        .with_sdp(ANSWER)
        .await;
    call.expect(183).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    h.advance(Duration::from_secs(releaser::FIRE_SEC as u64 + 1)).await;
    alice.drain().await;
    let mut prack = bob.receive("PRACK").await;
    let body = String::from_utf8_lossy(prack.request().body()).into_owned();
    assert!(body.lines().any(|l| l.starts_with("m=audio 0 ")), "rejecting answer: {body:?}");
    prack.respond(200, "OK").await;
    let ack = bob.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "the answer rode the PRACK");
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq(), "the 2xx's INVITE");
    bob.receive("BYE").await.respond(200, "OK").await;
    alice.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A service ends an answered callee before the caller ACKed the 2xx: the
/// UAC core ACKs every 2xx it receives (RFC 3261 §13.2.2.4), so the callee
/// sees this stack's own bare ACK — the INVITE carried the offer — on the
/// INVITE's CSeq, then the `BYE`.
#[tokio::test(start_paused = true)]
async fn a_callee_destroyed_before_the_callers_ack_is_acked_before_its_bye() {
    let h = Harness::new("destroy-leg-unacked-2xx");
    h.waive(
        scenario_harness::WaiverScope::rule(
            "no-ack-to-dialog-creating-2xx",
            "alice has not ACKed when the service fires",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:7453").await;
    let bob = h.agent("bob", "127.0.0.1:7454").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 7454)))
            .services(vec![releaser::service_def()])
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:7455")
            .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    h.advance(Duration::from_secs(releaser::FIRE_SEC as u64 + 1)).await;
    alice.drain().await;
    let ack = bob.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "the INVITE carried the offer");
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq(), "the 2xx's INVITE");
    bob.receive("BYE").await.respond(200, "OK").await;
    alice.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
