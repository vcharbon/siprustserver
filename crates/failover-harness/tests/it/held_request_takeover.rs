//! **A request a service holds for a later final, across a takeover**
//! (ADR-0055 §2). The primary holds the caller's INFO and arms the timer
//! whose turn answers it; both replicate. The primary dies; a datagram of the
//! call takes it over on the survivor, which rebuilds the held request's
//! server transaction from the record. A retransmission of the INFO is
//! absorbed there, never held a second time; the restored timer's turn
//! answers the caller on that transaction, and a copy of the INFO arriving
//! after draws that final again (RFC 3261 §17.2.2).
//!
//! ```text
//!   alice ─INFO─▶ primary                (held, timer armed, replicated)
//!               ✗ primary killed ✗
//!   alice ─INFO (copy)─▶ SURVIVOR        (absorbed by the seeded server transaction)
//!   timer fires on SURVIVOR ─202─▶ alice
//! ```

use std::time::Duration;

use failover_harness::FailoverHarness;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::Method;
use sip_message::{SipMessage, SipParser};

use crate::departure_support::bring_up_reactive;
use crate::inflight_resend::{
    assert_not_taken_over, assert_taken_over_once, establish, kill_serving_node, last_request_sent,
    reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of, synchronized_call_ref,
    REPLICATE,
};

/// The caller's INFO is held; the service's own timer answers it `202`
/// `ANSWER_AFTER_SEC` later.
mod holdinfo {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, TimerKey, TimerType};

    pub const ANSWER_AFTER_SEC: i64 = 3;
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
                    body: Vec::new(),
                    content_type: None,
                }]))
            },
        }
    }
}

fn services() -> Vec<b2bua::rules::ServiceDef> {
    vec![holdinfo::service_def()]
}

/// The held INFO's retransmission is absorbed on the survivor, which holds
/// it once and answers it `202` on the restored timer's turn.
#[tokio::test(start_paused = true)]
async fn a_held_request_is_answered_by_the_takeover_copy() {
    let mut fh =
        FailoverHarness::new("held-request-takeover", &["b1", "b2"]).with_worker_services(services);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // The caller's Timer E rung takes the call over and is absorbed there.
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Info);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_taken_over_once(survivor, hydrated_before);
    let copy = survivor.live_call(&call_ref).expect("the survivor serves the call");
    let held =
        copy.a_leg.dialogs.iter().flat_map(|d| d.requests.iter()).filter(|r| r.is_held()).count();
    assert_eq!(held, 1, "the copy holds the INFO once: its retransmission is absorbed");

    info.expect(202).await;
    assert_eq!(bob.drain_tu().await, 0, "nothing reaches the callee");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The finals put on the wire toward `to` with status `status`.
fn finals_toward(fh: &FailoverHarness, to: std::net::SocketAddr, status: u16) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter(
            |e| matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r)) if r.status() == status),
        )
        .count()
}

/// The caller's next request — a MESSAGE, relayed to the callee — takes the
/// call over; the survivor answers the held INFO on the restored timer's turn
/// through the transaction it rebuilt, and the caller's copy of the INFO sent
/// after draws that final again at once, never held a second time.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_held_request_the_takeover_answered_draws_its_final_again() {
    let mut fh = FailoverHarness::new("held-request-takeover-repeat", &["b1", "b2"])
        .with_worker_services(services);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    let mut message = dialog.send_request(InDialogMethod::Message).send().await;
    bob.receive("MESSAGE").await.respond(200, "OK").await;
    message.expect(200).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    info.expect(202).await;
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Info);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    fh.advance(Duration::from_millis(100)).await;
    assert_eq!(finals_toward(&fh, alice.addr(), 202), 2, "the copy draws the final again");
    let copy = survivor_of(&pri_ord, &w_b1, &w_b2).live_call(&call_ref).expect("served");
    let held =
        copy.a_leg.dialogs.iter().flat_map(|d| d.requests.iter()).filter(|r| r.is_held()).count();
    assert_eq!(held, 0, "the copy holds nothing: the INFO's copy never reached the service");
    alice.drain().await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
