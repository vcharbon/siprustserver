//! A client transaction that times out on a leg already ended.
//!
//! A peer's BYE ends the INVITE usage of its dialog; a subscription usage may
//! outlive it (RFC 5057 §5.2), so a service can still send a NOTIFY on that
//! leg after answering the BYE. When the peer never answers it, the NOTIFY's
//! transaction times out (Timer F, RFC 3261 §17.1.2.2) on a leg whose
//! disposition is already terminal: the leg keeps the disposition its BYE gave
//! it and the rest of the call stays up. A request of the INVITE usage toward
//! that leg is never relayed: this stack answers it 481 (RFC 3261 §12.2.2).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::ByeDisposition;
use scenario_harness::dialogs::DIALOG_ENDED;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ORDINAL: &str = "w0";

/// One rule: the callee's BYE is answered 200, the leg's subscription ended
/// by a terminating NOTIFY, and the leg alone terminated — the caller stays.
mod lingering {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use call::{ByeDisposition, Direction};

    fn on_callee_bye(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let leg = ctx.source_leg_id.to_string();
        Some(RuleHandleResult::new(vec![
            RuleAction::Respond {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
            },
            RuleAction::SendNotify {
                leg_id: leg.clone(),
                event: "refer".into(),
                subscription_state: "terminated;reason=noresource".into(),
                content_type: Some("message/sipfrag;version=2.0".into()),
                body: b"SIP/2.0 487 Request Terminated\r\n".to_vec(),
            },
            RuleAction::TerminateLeg {
                leg_id: leg,
                bye_disposition: Some(ByeDisposition::ByeReceived),
            },
        ]))
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "lingering-callee-bye",
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::request().method("BYE").direction(Direction::FromB),
            handle: on_callee_bye,
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

    pub fn service_def() -> ServiceDef {
        ServiceDef { id: "lingering", init, rules }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// The callee BYEs, then never answers the NOTIFY sent on its ended leg: past
/// Timer F the leg is still `ByeReceived` and the call still up; the caller's
/// BYE then ends it normally.
#[tokio::test(start_paused = true)]
async fn a_transaction_timing_out_on_an_ended_leg_keeps_its_disposition() {
    let h = Harness::new("ended-leg-txn-timeout");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(
        WaiverScope::rule(
            DIALOG_ENDED,
            "the test's own service sends the NOTIFY after bob's BYE: its timeout is the subject",
        )
        .on_party("bob"),
    );
    let b2bua = B2buaSut::builder(decision())
        .services(vec![lingering::service_def()])
        // No keepalive inside the Timer F window: the caller side answers none.
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let call_ref = call::derive_call_ref(ORDINAL, &call.call_id(), alice_dialog.local_tag());

    // The callee hangs up and ignores the NOTIFY that follows its BYE's 200.
    let mut bob_dialog = b_inv.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    bob.receive("NOTIFY").await;

    // Past Timer F (64·T1 = 32 s): the NOTIFY's transaction has timed out.
    h.advance(Duration::from_secs(33)).await;
    bob.drain().await; // the NOTIFY's retransmissions
    let live = b2bua.live_call(&call_ref).expect("the caller's side of the call is live");
    let callee = live.b_legs.iter().find(|l| l.leg_id == "b-1").expect("the callee's leg");
    assert_eq!(
        callee.bye_disposition,
        Some(ByeDisposition::ByeReceived),
        "a timeout on an ended leg leaves the disposition its BYE gave it",
    );
    assert_eq!(b2bua.active_calls(), 1, "the call stays up on the caller's side");

    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// The caller's in-dialog INFO, sent while the NOTIFY on the callee's ended
/// leg is unanswered, belongs to the INVITE usage the callee's BYE ended
/// (RFC 5057 §5.2): this stack answers it 481 itself (RFC 3261 §12.2.2) and
/// nothing reaches the callee. The NOTIFY's own timeout still leaves the leg
/// `ByeReceived`.
#[tokio::test(start_paused = true)]
async fn a_request_toward_a_leg_a_bye_ended_is_answered_481_here() {
    let h = Harness::new("ended-leg-txn-timeout-pending");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(
        WaiverScope::rule(
            DIALOG_ENDED,
            "the test's own service sends the NOTIFY after bob's BYE: its timeout is the subject",
        )
        .on_party("bob"),
    );
    let b2bua = B2buaSut::builder(decision())
        .services(vec![lingering::service_def()])
        // No keepalive inside the Timer F window: the caller side answers none.
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let call_ref = call::derive_call_ref(ORDINAL, &call.call_id(), alice_dialog.local_tag());

    let mut bob_dialog = b_inv.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    bob.receive("NOTIFY").await;

    // One second later the caller's INFO names the callee's ended dialog.
    h.advance(Duration::from_secs(1)).await;
    let mut info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    info.expect(481).await;

    // Past the NOTIFY's Timer F (32 s after the BYE).
    h.advance(Duration::from_secs(33)).await;
    assert_eq!(bob.drain_tu().await, 0, "nothing new reaches the callee");
    assert!(
        !bob.wire_view().iter().any(|e| e.raw.starts_with(b"INFO ")),
        "the INFO never reaches the callee",
    );
    let live = b2bua.live_call(&call_ref).expect("the caller's side of the call is live");
    let callee = live.b_legs.iter().find(|l| l.leg_id == "b-1").expect("the callee's leg");
    assert_eq!(callee.bye_disposition, Some(ByeDisposition::ByeReceived));

    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// What the SUT sends onto a dialog a BYE ended reaches the callee's dialog
/// table even when no step pulls it: the drained NOTIFY fails the run.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "dialog rule dialog-ended fired on bob")]
async fn a_drained_request_on_an_ended_dialog_fails_the_run() {
    let h = Harness::new("ended-leg-drained-notify");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![lingering::service_def()])
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = b_inv.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(bob.drain().await >= 1, "the NOTIFY reached bob");
    h.finish().await;
}
