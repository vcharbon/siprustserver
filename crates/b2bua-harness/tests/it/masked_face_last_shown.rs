//! A masking policy shows the caller two callee dialogs under one
//! caller-facing tag (a second leg rings behind the first). A request she
//! sends there goes to the callee dialog whose provisional she was last shown
//! under it: the dialog her view holds (RFC 3261 §12.1.1).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to_with_18x_messages;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::features::{Relay18xMessages, RelayFirst18xStrategy};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Where the second leg dials.
const SECOND_PORT: u16 = 5482;

/// The fork-out service: on a deadline it dials a second leg beside the
/// first, both ringing.
mod fork_out {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};
    use sip_message::Method;

    pub const AT_SEC: i64 = 5;
    const DEADLINE: TimerType = TimerType::service(FORK_OUT, "deadline");

    define_service! {
        id: "fork-out",
        machine: FORK_OUT,
        states: ForkOutState { Armed, Dialled },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(ForkOutState::Armed.label()).with_actions(vec![
                RuleAction::arm(TimerKey::call(DEADLINE), TimerDelay::secs(AT_SEC)),
            ]))
        },
        rules: [ dial_on_deadline() ],
    }

    fn dial_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "fork-out-on-deadline",
            machine: FORK_OUT,
            active: [ ForkOutState::Armed ],
            transitions: [ ForkOutState::Armed => ForkOutState::Dialled ],
            effects: [ Effect::Originate { method: Method::Invite, label: "INVITE → the second leg" } ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::CreateLeg {
                        destination: ("127.0.0.1".into(), super::SECOND_PORT),
                        new_ruri: None,
                        new_from: None,
                        new_to: None,
                        no_answer: call::Deadline::Off,
                        callback_context: None,
                        body_override: None,
                        header_updates: vec![],
                        header_adds: vec![],
                        kind: None,
                        attempt: None,
                    },
                    RuleAction::SetState { machine: FORK_OUT, to: ForkOutState::Dialled.label() },
                ]))
            },
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_caller_request_on_a_masked_face_goes_to_the_fork_she_was_last_shown() {
    let h = Harness::with_transit_delay("masked-face-last-shown", 1);
    let alice = h.agent_auto("alice").await;
    let bob1 = h.agent_auto("bob1").await;
    let bob2 = h.agent("bob2", &format!("127.0.0.1:{SECOND_PORT}")).await;
    let first = bob1.addr();
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to_with_18x_messages(
                    &first.ip().to_string(),
                    first.port(),
                    RelayFirst18xStrategy::DropSdp,
                    Relay18xMessages::All,
                ))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![fork_out::service_def()])
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    let shown1 = call.expect(180).await;
    h.advance(Duration::from_secs(fork_out::AT_SEC as u64) + Duration::from_millis(300)).await;
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(180, "Ringing").await;
    let shown2 = call.expect(180).await;
    assert_eq!(shown1.to().tag(), shown2.to().tag(), "both forks are shown under one tag");

    let mut info = call.send_request(InDialogMethod::Info).send().await;
    bob2.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    h.advance(Duration::from_millis(100)).await;
    assert!(bob1.try_receive_tolerating("INFO", &[]).await.is_none(), "the first fork took none");

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    for (bob, uas) in [(&bob1, &mut uas1), (&bob2, &mut uas2)] {
        bob.receive("CANCEL").await.respond(200, "OK").await;
        uas.respond(487, "Request Terminated").await;
        bob.receive("ACK").await;
    }

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
