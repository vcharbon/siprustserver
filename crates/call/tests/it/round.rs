//! How one copy's view of a re-INVITE round's end lands in the other
//! (`call::helpers::merge_rounds`): the round's own facts come in, every
//! other fact of the record that held it unfinished stays.

use crate::common;
use call::helpers::merge_rounds;
use call::model::{
    Call, Dialog, Direction, Obligation, PendingRequest, Repeated, RetainedEmission, TimerEntry,
    TimerKey, TimerType, Unacked2xx,
};
use call::testkit::{stage_reinvite_round, RoundStage, STAGED_ROUND_CSEQ};

const ROUND: &str = "z9hG4bK-round-1";

/// The answered representative call, the caller's re-INVITE round on the
/// callee's dialog standing at `stage`.
fn round(stage: RoundStage) -> Call {
    let mut call = common::representative_call();
    stage_reinvite_round(&mut call.b_legs[0], ROUND, stage);
    call
}

fn caller(call: &Call) -> &Dialog {
    call.a_leg.dialogs.confirmed().expect("the caller's dialog")
}

fn callee(call: &Call) -> &Dialog {
    call.b_legs[0].dialogs.confirmed().expect("the callee's dialog")
}

/// The round's 2xx awaits the caller's ACK on her dialog, its ladder armed.
fn awaiting_the_callers_ack(mut call: Call) -> Call {
    let d = call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    let tag = d.our_tag.clone();
    d.pending_reinvite_2xx = Some(Unacked2xx {
        dialog_tag: tag.clone(),
        cseq: STAGED_ROUND_CSEQ,
        emission: RetainedEmission::on_trigger(
            b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
            ("192.0.2.10".into(), 5060),
            Repeated::response("INVITE", 200),
        ),
    });
    let obligation =
        Obligation::AckOf2xx { leg: "a".into(), dialog_tag: tag, cseq: STAGED_ROUND_CSEQ };
    call.timers.push(TimerEntry {
        key: TimerKey::obligation(TimerType::Rung, obligation),
        generation: 9,
        fire_at: 500,
    });
    call
}

fn ladders(call: &Call) -> usize {
    call.timers
        .iter()
        .filter(|t| matches!(t.key.obligation_of(), Some(Obligation::AckOf2xx { cseq, .. }) if *cseq == STAGED_ROUND_CSEQ))
        .count()
}

/// A request the record relayed toward the caller on its own.
fn info_toward_the_caller(call: &mut Call) {
    let d = call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.our_cseq += 1;
    d.inbound_pending_requests.push(PendingRequest {
        method: "INFO".into(),
        outbound_cseq: d.our_cseq,
        inbound_cseq: 2001,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        direction: Direction::FromB,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
    });
}

#[test]
fn an_answered_round_comes_in_and_the_records_own_request_stays() {
    let mut held = round(RoundStage::Open);
    info_toward_the_caller(&mut held);
    let incoming = awaiting_the_callers_ack(round(RoundStage::Answered));

    let merged = merge_rounds(held.clone(), &incoming);

    assert_eq!(callee(&merged).awaited_ack_cseq, Some(STAGED_ROUND_CSEQ), "the ACK is awaited");
    assert!(callee(&merged).inbound_pending_requests.iter().all(|p| p.method != "INVITE"));
    assert_eq!(caller(&merged).pending_reinvite_2xx, caller(&incoming).pending_reinvite_2xx);
    assert_eq!(ladders(&merged), 1, "the 2xx's ladder came in");
    assert_eq!(caller(&merged).our_cseq, caller(&held).our_cseq, "the record's own sequence");
    assert!(
        caller(&merged).inbound_pending_requests.iter().any(|p| p.method == "INFO"),
        "the record's own INFO stays pending"
    );
}

#[test]
fn the_dialog_sequence_is_never_lowered() {
    let mut held = round(RoundStage::Open);
    held.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog").our_cseq += 1;
    let incoming = round(RoundStage::Answered);
    let merged = merge_rounds(held.clone(), &incoming);
    assert_eq!(callee(&merged).our_cseq, callee(&held).our_cseq);
    assert_eq!(callee(&merged).awaited_ack_cseq, Some(STAGED_ROUND_CSEQ));
}

#[test]
fn a_refused_round_leaves_nothing_open() {
    let merged = merge_rounds(round(RoundStage::Open), &round(RoundStage::Refused));
    let d = callee(&merged);
    assert!(d.pending_invite_txn.is_none() && d.awaited_ack_cseq.is_none());
    assert!(d.inbound_pending_requests.iter().all(|p| p.method != "INVITE"));
}

#[test]
fn an_acked_round_retires_the_ladder_the_record_held() {
    let held = awaiting_the_callers_ack(round(RoundStage::Answered));
    let merged = merge_rounds(held, &round(RoundStage::Acked));
    assert!(callee(&merged).awaited_ack_cseq.is_none() && callee(&merged).ack_branch.is_some());
    assert!(caller(&merged).pending_reinvite_2xx.is_none(), "the 2xx is ACKed");
    assert_eq!(ladders(&merged), 0, "its ladder goes with it");
}

#[test]
fn a_round_the_other_copy_never_saw_end_changes_nothing() {
    let held = round(RoundStage::Open);
    let before = common::representative_call();
    assert_eq!(merge_rounds(held.clone(), &before), held);
    assert_eq!(merge_rounds(held.clone(), &round(RoundStage::Open)), held);
}
