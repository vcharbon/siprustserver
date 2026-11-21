//! A 2xx to a re-INVITE this stack sent on its own account, arriving after
//! the INVITE's Timer B, is dropped (RFC 6026 §7.2): the client transaction
//! that gave up matches it no more, so it never reaches the TU and the ACK of
//! RFC 3261 §13.2.2.4 is not owed. A service re-INVITEs the callee and keeps
//! the call when that re-INVITE times out; the callee, silent past Timer B,
//! then answers it 200. No ACK leaves, nothing reaches the caller, neither
//! leg reads it and the call stays up. The callee, never ACKed, re-sends it
//! to 64·T1 and clears the dialog with a BYE (§13.3.1.4): one CDR, fully
//! reaped.
//!
//! The 200 leaves once while the layer still holds the given-up transaction
//! (64·T1 past Timer B, for the final a CANCEL may draw) and once after it
//! let it go: dropped either way.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

use crate::common::late_2xx::{
    assert_legs_unchanged, last_2xx, legs, reading_every_message, repeat_on_ladder, sent_to,
    LADDER_MS,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// The caller's next description, which the service offers the callee.
const ALICE_V2: &str = "v=0\r\no=alice 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_V2: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// When the service re-INVITEs the callee, after the call is set up.
const RENEGOTIATE_SEC: u64 = 4;

/// When the callee's late 200 leaves after the re-INVITE: a second past its
/// Timer B (64·T1 = 32 s), the given-up transaction still held.
const PAST_TIMER_B: Duration = Duration::from_secs(33);

/// A second past the hold of the given-up transaction (64·T1 past Timer B).
const PAST_THE_HOLD: Duration = Duration::from_secs(65);

/// On its timer, re-INVITEs the callee with the caller's next description;
/// ACKs the 2xx that answers it, and keeps the call when it times out.
mod renegotiate {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use b2bua_sdk::model::Body;
    use call::{Direction, TimerKey, TimerType};
    use sip_message::Method;

    const RENEGOTIATE: TimerType = TimerType::service(RENEGOTIATE_MACHINE, "renegotiate");

    define_service! {
        id: "renegotiate",
        machine: RENEGOTIATE_MACHINE,
        states: RenegotiateState { Armed, Renegotiating },
        init: arm,
        rules: [ fire(), answered(), timed_out() ],
    }

    fn arm(_call: &RuleCall) -> Option<ServiceSeed> {
        Some(ServiceSeed::new(RenegotiateState::Armed.label()).with_actions(vec![RuleAction::arm(
            TimerKey::call(RENEGOTIATE),
            TimerDelay::secs(super::RENEGOTIATE_SEC as i64),
        )]))
    }

    fn fire() -> RuleDefinition {
        sm_rule! {
            id: "renegotiate-fire",
            machine: RENEGOTIATE_MACHINE,
            active: [ RenegotiateState::Armed ],
            transitions: [ RenegotiateState::Armed => RenegotiateState::Renegotiating ],
            effects: [ Effect::Originate { method: Method::Invite, label: "re-INVITE → callee" } ],
            matcher: Match::timer().timer_type(RENEGOTIATE),
            handle: |ctx: &RuleContext| {
                let callee = ctx.call.b_legs().first()?.leg_id.clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::SendReinvite {
                        leg_id: callee,
                        body: Some(Body::from_leg(super::ALICE_V2.as_bytes().to_vec(), "a")),
                        add_headers: vec![],
                    },
                    RuleAction::SetState {
                        machine: RENEGOTIATE_MACHINE,
                        to: RenegotiateState::Renegotiating.label(),
                    },
                ]))
            },
        }
    }

    fn answered() -> RuleDefinition {
        sm_rule! {
            id: "renegotiate-answered",
            machine: RENEGOTIATE_MACHINE,
            active: [ RenegotiateState::Renegotiating ],
            transitions: [ RenegotiateState::Renegotiating => Terminal ],
            effects: [ Effect::Originate { method: Method::Ack, label: "ACK → callee" } ],
            matcher: Match::response().method("INVITE").status_class(2).direction(Direction::FromB),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::AckLeg { leg_id: ctx.source_leg_id.to_string(), body: None },
                    RuleAction::ClearState { machine: RENEGOTIATE_MACHINE },
                ]))
            },
        }
    }

    /// The timeout of the re-INVITE [`fire`] sent, the one INVITE in progress
    /// while the service renegotiates (RFC 3261 §14.1): the call stays.
    fn timed_out() -> RuleDefinition {
        sm_rule! {
            id: "renegotiate-timed-out",
            machine: RENEGOTIATE_MACHINE,
            active: [ RenegotiateState::Renegotiating ],
            transitions: [ RenegotiateState::Renegotiating => Terminal ],
            effects: [],
            matcher: Match::timeout()
                .filter(|ctx: &RuleContext| ctx.timeout_method().is_some_and(|m| m == "INVITE")),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![RuleAction::ClearState {
                    machine: RENEGOTIATE_MACHINE,
                }]))
            },
        }
    }
}

/// The given-up transaction is still held when the late 200 arrives.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_an_own_reinvite_after_its_timer_b_is_dropped() {
    the_late_2xx_is_dropped("b2bua-reinvite-2xx-after-timer-b", PAST_TIMER_B).await;
}

/// The layer let the given-up transaction go before the late 200 arrives.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_an_own_reinvite_whose_transaction_is_gone_is_dropped() {
    the_late_2xx_is_dropped("b2bua-reinvite-2xx-past-its-transaction", PAST_THE_HOLD).await;
}

/// The callee answers the service's re-INVITE 200 `late_by` after it.
async fn the_late_2xx_is_dropped(name: &str, late_by: Duration) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .services(vec![renegotiate::service_def()])
        .tune(reading_every_message)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let call_ref = call::derive_call_ref("w0", &call.call_id(), alice_dialog.local_tag());

    h.advance(Duration::from_secs(RENEGOTIATE_SEC)).await;
    let mut unanswered = bob.receive("INVITE").await;
    h.advance(PAST_TIMER_B).await;
    bob.drain().await;
    let kept = b2bua.live_call(&call_ref).expect("the service keeps the call");
    assert_eq!(kept.state, call::CallModelState::Active, "the timeout leaves the call up");
    h.advance(late_by - PAST_TIMER_B).await;
    let kept = b2bua.live_call(&call_ref).expect("the call is live");

    let before = h.wire_entries().len();
    let held = legs(&kept);
    unanswered.respond(200, "OK").with_sdp(BOB_V2).await;
    let late = last_2xx(&h, bob.addr(), b2bua.addr);
    let to_give_up = repeat_on_ladder(&h, &bob, &late, b2bua.addr).await;
    assert_eq!(
        sent_to(&h, b2bua.addr, bob.addr(), before),
        Vec::<String>::new(),
        "no ACK answers the late 200 or any of its {} copies",
        LADDER_MS.len()
    );
    assert_eq!(
        sent_to(&h, b2bua.addr, alice.addr(), before),
        Vec::<String>::new(),
        "the late 200 reaches no one"
    );
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(live.state, call::CallModelState::Active, "the late 200 leaves the call up");
    assert_legs_unchanged(&held, &live);

    h.advance(to_give_up).await;
    let mut bye = uas.dialog().bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}
