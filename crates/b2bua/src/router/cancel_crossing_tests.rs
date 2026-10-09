//! The caller's CANCEL beats the callee's 2xx to her at this stack: the turn on
//! the callee's 200 recorded and built the relayed 2xx, but the transaction
//! layer had already answered her CANCEL (200) and her INVITE (487, RFC 3261
//! §9.2), so the completed transaction never sent that 2xx. The `Cancelled`
//! turn brings the record back to the 487 that went out: no BYE toward the
//! caller, whose INVITE holds no dialog (§15), while the answered callee gets
//! its BYE; and the answer stays on the call's lifecycle axis, so replication
//! never reads the teardown as a branch behind the answered copy.

use call::helpers::{make_empty_dialog, MakeDialogLegCtx};
use call::{Call, Direction, LegState};

use super::process::rule_chain_turn;
use super::resolve::Resolution;
use super::test_support::{invite, node, src};
use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::initial_invite::build_initial_call;
use b2bua_sdk::event::CallEvent;

/// The call as the turn on bob's 200 left it: the callee leg `b-1` confirmed,
/// and the relayed 2xx recorded on the caller's leg (her dialog confirmed
/// under the a-facing tag) though it never reached the wire.
fn relayed_answer_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "cancel-crossing"),
        src(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.a_leg.dialogs = vec![make_empty_dialog(
        &MakeDialogLegCtx {
            call_id: &call.a_leg.call_id.clone(),
            local_uri: "sip:bob@example.com",
            remote_uri: "sip:alice@example.com",
            local_tag: "a1",
            remote_tag: &call.a_leg.from_tag.clone(),
        },
        1,
    )];
    call.a_leg.dialogs[0].sip.remote_target = "sip:alice@10.0.0.9:5060".into();
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.invite_final_sent = Some(200);
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = "b-cid@10.0.0.2".into();
    b.from_tag = "b2bua-b-tag".into();
    b.invite_final_sent = None;
    b.dialogs = vec![make_empty_dialog(
        &MakeDialogLegCtx {
            call_id: "b-cid@10.0.0.2",
            local_uri: "sip:alice@example.com",
            remote_uri: "sip:bob@example.com",
            local_tag: "b2bua-b-tag",
            remote_tag: "bob-tag",
        },
        1,
    )];
    b.dialogs[0].sip.remote_target = "sip:bob@10.0.0.2:5060".into();
    call::helpers::add_b_leg(call, b)
}

#[tokio::test(start_paused = true)]
async fn a_cancel_that_beat_the_relayed_2xx_byes_the_callee_only() {
    let node = node("w0").await;
    let call = relayed_answer_call();
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg.from_tag.clone(),
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
    let result = rule_chain_turn(node.core.router_ctx(), call, &event, &res, &call_ref, 1_000);

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
    assert_ne!(result.call.a_leg.state, LegState::Confirmed, "the caller holds no dialog");
    assert!(!call::helpers::caller_answered(&result.call), "the caller was never answered");
    assert_eq!(
        call::helpers::lifecycle_position(&result.call).0,
        1,
        "the pre-empted answer stays on the lifecycle's answer axis"
    );
}
