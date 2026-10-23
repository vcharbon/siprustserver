//! A caller request whose peer dialog this stack's BYE is ending (a service
//! released the callee mid-call): the caller's own dialog lives, so she is
//! told to retry (500 with `Retry-After`, RFC 3261 §14.2), never 481, which
//! would end her dialog (§12.2.1.2); an UPDATE keeps its 491 (RFC 3311 §5.2).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, stated_by_response, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The release service: on a deadline it releases the callee leg, the
/// caller's leg staying up.
mod release {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};
    use sip_message::Method;

    pub const AT_SEC: i64 = 10;
    const DEADLINE: TimerType = TimerType::service(RELEASE, "deadline");

    define_service! {
        id: "release-callee",
        machine: RELEASE,
        states: ReleaseState { Armed, Released },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(ReleaseState::Armed.label()).with_actions(vec![
                RuleAction::arm(TimerKey::call(DEADLINE), TimerDelay::secs(AT_SEC)),
            ]))
        },
        rules: [ release_on_deadline() ],
    }

    fn release_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "release-callee-on-deadline",
            machine: RELEASE,
            active: [ ReleaseState::Armed ],
            transitions: [ ReleaseState::Armed => ReleaseState::Released ],
            effects: [ Effect::Originate { method: Method::Bye, label: "BYE → the callee" } ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::DestroyLeg { leg_id: "b-1".into(), headers: vec![] },
                    RuleAction::SetState { machine: RELEASE, to: ReleaseState::Released.label() },
                ]))
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_caller_request_toward_a_peer_dialog_being_ended_is_told_to_retry() {
    let h = Harness::with_transit_delay("peer-dialog-ending", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to(
        bob.addr().ip().to_string(),
        bob.addr().port(),
    ));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![release::service_def()])
        .tune(|c| {
            c.setup_timeout_ms = 300_000;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(release::AT_SEC as u64) + Duration::from_millis(300)).await;
    let mut released = bob.receive("BYE").await;

    let info = dialog.send_request(InDialogMethod::Info).send().await.expect(500).await;
    assert!(
        stated_by_response(&info, "Retry-After").is_some_and(|v| v.trim() == "1"),
        "the 500 states Retry-After: 1"
    );
    dialog.send_request(InDialogMethod::Update).send().await.expect(491).await;
    h.advance(Duration::from_millis(100)).await;
    assert!(bob.try_receive_tolerating("INFO", &[]).await.is_none(), "nothing crossed");

    released.respond(200, "OK").await;
    let mut bye = dialog.bye().await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
