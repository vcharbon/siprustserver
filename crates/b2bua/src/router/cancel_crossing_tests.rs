//! The caller's CANCEL beats the callee's 2xx to her at this stack: the turn on
//! the callee's 200 recorded and built the relayed 2xx, but the transaction
//! layer had already answered her CANCEL (200) and her INVITE (487, RFC 3261
//! §9.2), so the completed transaction never sent that 2xx. The `Cancelled`
//! turn brings the record back to the 487 that went out: no BYE toward the
//! caller, whose INVITE holds no dialog (§15), while the answered callee gets
//! its BYE; and the answer stays on the call's lifecycle axis, so replication
//! never reads the teardown as a branch behind the answered copy.

use call::{
    Call, Dialog, DialogState, Direction, LegState, Obligation, OutboundInvite, Repeated,
    RetainedEmission, TimerEntry, TimerType, Unacked2xx,
};
use sip_retransmit::Class;

use super::process::rule_chain_turn;
use super::resolve::Resolution;
use super::test_support::{invite, node, src};
use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::initial_invite::build_initial_call;
use b2bua_sdk::event::CallEvent;

/// The call as the turn on bob's 200 left it: the callee leg `b-1` confirmed,
/// and the relayed 2xx recorded on the caller's leg (her dialog confirmed
/// under the a-facing tag, its §13.3.1.4 ladder armed) though it never
/// reached the wire.
fn relayed_answer_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "cancel-crossing"),
        src(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    let mut a1 = Dialog::new("a1", call.a_leg_invite.from_tag.clone(), DialogState::Confirmed, 1);
    a1.remote_target = "sip:alice@10.0.0.9:5060".into();
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.invite_final_sent = Some(200);
    let (emission, _) = RetainedEmission::paced(
        b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
        ("10.0.0.9".into(), 5060),
        Class::Final2xx,
        Repeated::response("INVITE", 200),
    );
    a1.answered_2xx = Some(Unacked2xx { dialog_tag: "a1".into(), cseq: 1, emission });
    call.a_leg.dialogs = vec![a1].into();
    call.a_leg.own_tag = Some("a1".into());
    let obligation = Obligation::AckOf2xx { leg: "a".into(), dialog_tag: "a1".into(), cseq: 1 };
    for timer_type in
        [TimerType::Rung { obligation: obligation.clone() }, TimerType::RepeatGiveUp { obligation }]
    {
        call.timers.push(TimerEntry {
            id: timer_type.timer_id(None),
            timer_type,
            fire_at: 1_500,
            leg_id: None,
        });
    }
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = "b-cid@10.0.0.2".into();
    b.outbound_invite = Some(OutboundInvite { our_tag: "b2bua-b-tag".into(), cseq: 1 });
    b.own_tag = None;
    b.invite_final_sent = None;
    let mut bob = Dialog::new("b2bua-b-tag", "bob-tag", DialogState::Confirmed, 1);
    bob.remote_target = "sip:bob@10.0.0.2:5060".into();
    b.dialogs = vec![bob].into();
    call::helpers::add_b_leg(call, b)
}

#[tokio::test(start_paused = true)]
async fn a_cancel_that_beat_the_relayed_2xx_byes_the_callee_only() {
    let node = node("w0").await;
    let call = relayed_answer_call();
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: Some(1),
        in_dialog: false,
        headers: vec![],
        to_tag: Some("a1".into()),
    };
    let res = Resolution {
        call_ref: Some(call.call_ref.clone()),
        source_leg_id: call.a_leg.leg_id.clone(),
        direction: Direction::FromA,
        initial_invite: false,
        incarnation: None,
        indexed: None,
    };
    let call_ref = call.call_ref.clone();
    let result = rule_chain_turn(
        node.core.router_ctx(),
        call,
        &event,
        &res,
        Default::default(),
        &call_ref,
        1_000,
    );

    let requests_to = |leg: &str| -> Vec<String> {
        result
            .effects
            .outbound
            .iter()
            .filter(|e| e.leg_id.as_deref() == Some(leg))
            .filter_map(|e| match &e.body {
                OutboundBody::Request(r) => Some(r.method().as_str().to_string()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(requests_to("a"), Vec::<String>::new(), "nothing goes to the caller");
    assert!(requests_to("b-1").iter().any(|m| m == "BYE"), "bob gets his BYE");
    assert_eq!(result.call.a_leg.invite_final_sent, Some(487), "the final the wire carries");
    assert_eq!(result.call.a_leg.state, LegState::Terminated, "the caller holds no dialog");
    assert!(!call::helpers::caller_answered(&result.call), "the caller was never answered");
    assert!(
        result.call.a_leg.dialogs.iter().all(|d| d.answered_2xx.is_none()),
        "no 2xx awaits the caller's ACK"
    );
    let ladders: Vec<_> = result
        .call
        .timers
        .iter()
        .filter(|t| matches!(t.timer_type, TimerType::Rung { .. } | TimerType::RepeatGiveUp { .. }))
        .map(|t| t.id.clone())
        .collect();
    assert_eq!(ladders, Vec::<String>::new(), "no ladder repeats a 2xx that never left");
    assert_eq!(
        call::helpers::lifecycle_position(&result.call).0,
        1,
        "the pre-empted answer stays on the lifecycle's answer axis"
    );
}
