//! A reliable provisional through the real turn (RFC 3262): the engine takes
//! it before the rules — a repeat or one out of `RSeq` order is dropped unread
//! — and settles the PRACK this stack owes after them; a PRACK owed on a
//! dialog the record cannot name is a model defect, never a silent waiver.

use call::{
    CSeq, Call, Dialog, DialogState, Direction, LegState, Provisional, ProvisionalId, RSeq,
    ResponderAck,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::test_support::{invite, node, src};
use crate::config::B2buaConfig;
use crate::effects::{HandlerResult, OutboundBody};
use crate::initial_invite::build_initial_call;
use crate::rule_chain::{rule_chain_turn, TurnInput};
use crate::rules::actions::{Owed, Settle};
use crate::rules::ActionExecutor;
use crate::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;

/// A call whose caller is `answered` or still ringing, with a callee leg `b-1`
/// dialled under the tag `svc` and no dialog yet.
fn call_with_a_ringing_callee(answered: bool) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "provisional-turn"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    if answered {
        let mut a1 =
            Dialog::new("a1", call.a_leg_invite.from_tag.clone(), DialogState::Confirmed, 1);
        a1.remote_target = "sip:alice@10.0.0.9:5060".into();
        call.a_leg.dialogs = vec![a1].into();
        call.a_leg.own_tag = Some("a1".into());
        call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
        *call.a_leg.invite_final_sent_mut() = Some(200);
    }
    let mut b = call::testkit::dialled_leg("b-1", "svc");
    b.call_id = "bcid@x".into();
    call::helpers::add_b_leg(call, b)
}

/// A reliable `status` from the callee `bobtag` on `b-1` stating `rseq`.
fn reliable(status: u16, rseq: u32) -> CallEvent {
    let raw = format!(
        "SIP/2.0 {status} Ringing\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Require: 100rel\r\nRSeq: {rseq}\r\n\
Content-Length: 0\r\n\r\n"
    );
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw.as_bytes()) else {
        panic!("a response")
    };
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// One real turn of `event` from `b-1` over `call`.
async fn turn(call: Call, event: &CallEvent) -> HandlerResult {
    let node = node("w0").await;
    let call_ref = call.call_ref.clone();
    let mut scope = super::test_support::turn(1);
    let input = TurnInput {
        event,
        call_ref: &call_ref,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        resolved: Default::default(),
        matched_client_txn: true,
        now_ms: 1_000,
        owners: Default::default(),
    };
    rule_chain_turn(&node.core.router_ctx().framework(), call, input, &mut scope)
}

/// The PRACKs `result` sends toward `b-1`, as their `RAck`.
fn pracks(result: &HandlerResult) -> Vec<String> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("b-1"))
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "PRACK" => r
                .raw_text(sip_message::header::HeaderName::from("RAck"))
                .next()
                .map(|v| v.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// The recorded provisional `rseq` of the callee `bobtag` on `b-1`'s INVITE.
fn recorded(rseq: u32) -> Provisional {
    Provisional {
        id: ProvisionalId {
            leg: "b-1".into(),
            responder_tag: "bobtag".into(),
            invite_cseq: CSeq(1),
            rseq: RSeq(rseq),
        },
        responder_sdp: false,
        responder: ResponderAck::OwnPrack,
        shown: None,
    }
}

/// The caller is answered, so a callee's reliable ring is shown to no one:
/// the turn PRACKs it on the early dialog it opened, whatever the rule made
/// of it (RFC 3262 §4), and the callee's retransmission of it draws nothing.
#[tokio::test(start_paused = true)]
async fn a_ring_shown_to_no_one_is_pracked_once_by_the_turn() {
    let event = reliable(180, 1);
    let first = turn(call_with_a_ringing_callee(true), &event).await;
    assert_eq!(pracks(&first), vec!["1 1 INVITE".to_string()], "the turn PRACKs the ring");
    assert_eq!(first.call.b_legs[0].state(), LegState::Early, "its early dialog is open");
    let repeat = turn(first.call, &event).await;
    assert!(pracks(&repeat).is_empty(), "a retransmission draws no second PRACK");
}

/// A ringing provisional out of its dialog's `RSeq` order is dropped before
/// anything reads it (RFC 3262 §4): no PRACK, and the callee is not recorded
/// as having rung.
#[tokio::test(start_paused = true)]
async fn an_out_of_order_ring_is_dropped_unread() {
    let mut call = call_with_a_ringing_callee(false);
    call.provisionals.push(recorded(1));
    let result = turn(call, &reliable(180, 3)).await;
    assert!(pracks(&result).is_empty(), "no PRACK for an out-of-order provisional");
    assert!(!result.call.b_legs[0].ring.rang(), "a dropped provisional rings nothing");
}

/// A PRACK owed on a responder dialog the leg does not hold — no early
/// dialog was opened for it — is a model defect, reported rather than waived.
#[test]
fn an_owed_prack_on_a_dialog_the_leg_lacks_is_a_defect() {
    let mut call = call_with_a_ringing_callee(true);
    let mut owed = recorded(1);
    owed.responder = ResponderAck::Pending;
    call.provisionals.push(owed);
    let config = B2buaConfig::default();
    let mut scope = TurnScope::of(sip_ids::TurnIds::fixed(1));
    let mut exec = ActionExecutor {
        owners: crate::owners::Owners::none(),
        config: &config,
        turn: &mut scope,
        now_ms: 0,
    };
    let mut fx = crate::effects::HandlerEffects::new();
    exec.settle_owed_pracks(&mut call, &mut fx, Owed::Call, Settle::TurnEnd);
    let defects: Vec<_> = scope.notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [crate::turn_notes::PRACK_HAS_A_DIALOG]);
    assert!(fx.outbound.is_empty(), "no PRACK leaves");
}

/// A PRACK owed on a dialog whose state makes no request (its remote target
/// fails the Request-URI checks) is not sent, and the call records nothing of
/// it: the provisional stays owed and the dialog spends no number.
#[test]
fn an_unbuilt_prack_records_nothing() {
    let mut call = call_with_a_ringing_callee(true);
    let mut early = Dialog::new("svc", "bobtag", DialogState::Early, 1);
    early.remote_target = "sip:bob@10.0.0.2:5070x".into();
    call.b_legs[0].dialogs.insert(early);
    let mut owed = recorded(1);
    owed.responder = ResponderAck::Pending;
    call.provisionals.push(owed);
    let config = B2buaConfig::default();
    let mut scope = TurnScope::of(sip_ids::TurnIds::fixed(1));
    let mut exec = ActionExecutor {
        owners: crate::owners::Owners::none(),
        config: &config,
        turn: &mut scope,
        now_ms: 0,
    };
    let mut fx = crate::effects::HandlerEffects::new();
    exec.settle_owed_pracks(&mut call, &mut fx, Owed::Call, Settle::TurnEnd);
    assert!(fx.outbound.is_empty(), "no PRACK leaves");
    assert_eq!(call.provisionals[0].responder, ResponderAck::Pending, "the PRACK is still owed");
    let spent = call.b_legs[0].dialogs.iter().map(|d| d.our_cseq).collect::<Vec<_>>();
    assert_eq!(spent, vec![1], "the dialog spends no number");
}
