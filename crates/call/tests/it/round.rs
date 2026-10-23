//! How one copy's view of a re-INVITE round's end lands in the other
//! (`call::helpers::merge_rounds`): the round's own facts come in, every
//! other fact of the record that held it unfinished stays.

use crate::common;
use call::helpers::{
    ack_branch, answered_2xx, awaited_ack_cseq, hold_answered_2xx, invite_relay, merge_rounds,
};
use call::model::{
    Call, Dialog, Obligation, OwedAnswer, RSeq, Repeated, RequestRecord, RetainedEmission,
    TimerEntry, TimerKey, TimerType, Unacked2xx,
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
    let answer = Unacked2xx {
        dialog_tag: tag.clone(),
        cseq: STAGED_ROUND_CSEQ,
        emission: RetainedEmission::on_trigger(
            b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
            ("192.0.2.10".into(), 5060),
            Repeated::response("INVITE", 200),
        ),
    };
    hold_answered_2xx(d, STAGED_ROUND_CSEQ, answer);
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
    d.requests.push(RequestRecord {
        method: "INFO".into(),
        outbound_cseq: d.our_cseq,
        inbound_cseq: 2001,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        origin: Default::default(),
        usage: Default::default(),
        sent: None,
        deadline_ms: None,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
        relayed_by: Default::default(),
        author: None,
        invite: None,
    });
}

#[test]
fn an_answered_round_comes_in_and_the_records_own_request_stays() {
    let mut held = round(RoundStage::Open);
    info_toward_the_caller(&mut held);
    let incoming = awaiting_the_callers_ack(round(RoundStage::Answered));

    let merged = merge_rounds(held.clone(), &incoming);

    assert_eq!(awaited_ack_cseq(callee(&merged)), Some(STAGED_ROUND_CSEQ), "the ACK is awaited");
    assert!(invite_relay(callee(&merged)).is_none());
    assert_eq!(answered_2xx(caller(&merged)), answered_2xx(caller(&incoming)));
    assert_eq!(ladders(&merged), 1, "the 2xx's ladder came in");
    assert_eq!(caller(&merged).our_cseq, caller(&held).our_cseq, "the record's own sequence");
    assert!(
        caller(&merged).requests.iter().any(|p| p.method == "INFO"),
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
    assert_eq!(awaited_ack_cseq(callee(&merged)), Some(STAGED_ROUND_CSEQ));
}

#[test]
fn a_refused_round_leaves_nothing_open() {
    let merged = merge_rounds(round(RoundStage::Open), &round(RoundStage::Refused));
    let d = callee(&merged);
    assert!(d.sent_invite.is_none() && awaited_ack_cseq(d).is_none());
}

#[test]
fn an_acked_round_retires_the_ladder_the_record_held() {
    let held = awaiting_the_callers_ack(round(RoundStage::Answered));
    let merged = merge_rounds(held, &round(RoundStage::Acked));
    assert!(awaited_ack_cseq(callee(&merged)).is_none() && ack_branch(callee(&merged)).is_some());
    assert!(answered_2xx(caller(&merged)).is_none(), "the 2xx is ACKed");
    assert_eq!(ladders(&merged), 0, "its ladder goes with it");
}

#[test]
fn a_round_the_other_copy_never_saw_end_changes_nothing() {
    let held = round(RoundStage::Open);
    let before = common::representative_call();
    assert_eq!(merge_rounds(held.clone(), &before), held);
    assert_eq!(merge_rounds(held.clone(), &round(RoundStage::Open)), held);
}

#[test]
fn a_round_the_record_never_reached_comes_in_and_the_records_own_request_stays() {
    let mut held = common::representative_call();
    info_toward_the_caller(&mut held);
    let mut incoming = round(RoundStage::Open);
    let theirs = incoming.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    theirs.peer_cseq = caller(&held).peer_cseq.map(|c| c + 1);

    let merged = merge_rounds(held.clone(), &incoming);

    assert_eq!(callee(&merged).sent_invite, callee(&incoming).sent_invite);
    assert!(invite_relay(callee(&merged)).is_some());
    assert_eq!(callee(&merged).our_cseq, callee(&incoming).our_cseq, "the round's CSeq");
    assert_eq!(caller(&merged).peer_cseq, caller(&incoming).peer_cseq, "the caller's re-INVITE");
    assert_eq!(caller(&merged).our_cseq, caller(&held).our_cseq, "the record's own sequence");
    assert!(
        caller(&merged).requests.iter().any(|p| p.method == "INFO"),
        "the record's own INFO stays pending"
    );
}

#[test]
fn an_acked_round_the_record_never_reached_comes_in_with_its_ack() {
    let held = common::representative_call();
    let incoming = round(RoundStage::Acked);
    let merged = merge_rounds(held, &incoming);
    assert!(ack_branch(callee(&merged)).is_some(), "a repeated 2xx is re-ACKed");
    assert_eq!(callee(&merged).sent_invite.as_ref().and_then(|r| r.branch()), Some(ROUND));
    assert_eq!(callee(&merged).our_cseq, callee(&incoming).our_cseq, "the round's CSeq");
}

/// A session state that says which description went last, and after how
/// many exchanges the leg's peer opened.
fn session_on(call: &mut Call, leg: usize, origin: &str, opened: u32) {
    let leg = if leg == 0 { &mut call.a_leg } else { &mut call.b_legs[leg - 1] };
    leg.sdp_session.sent_origin = Some(origin.into());
    leg.sdp_session.exchanges_opened = opened;
}

fn sent_origin(call: &Call, leg: usize) -> Option<&str> {
    let leg = if leg == 0 { &call.a_leg } else { &call.b_legs[leg - 1] };
    leg.sdp_session.sent_origin.as_deref()
}

#[test]
fn the_rounds_session_comes_in() {
    let mut held = round(RoundStage::Open);
    session_on(&mut held, 0, "b2bua 1 1 IN IP4 192.0.2.1", 1);
    let mut incoming = round(RoundStage::Answered);
    session_on(&mut incoming, 0, "b2bua 1 2 IN IP4 192.0.2.1", 1);
    let merged = merge_rounds(held, &incoming);
    assert_eq!(
        sent_origin(&merged, 0),
        Some("b2bua 1 2 IN IP4 192.0.2.1"),
        "the 2xx's description"
    );
}

#[test]
fn a_session_the_record_moved_on_its_own_stays() {
    let mut held = round(RoundStage::Answered);
    session_on(&mut held, 0, "b2bua 1 3 IN IP4 192.0.2.1", 1);
    session_on(&mut held, 1, "b2bua 2 3 IN IP4 192.0.2.1", 2);
    let mut incoming = round(RoundStage::Acked);
    session_on(&mut incoming, 0, "b2bua 1 2 IN IP4 192.0.2.1", 1);
    session_on(&mut incoming, 1, "b2bua 2 2 IN IP4 192.0.2.1", 1);
    let merged = merge_rounds(held, &incoming);
    assert!(ack_branch(callee(&merged)).is_some(), "the round's end came in");
    assert_eq!(sent_origin(&merged, 0), Some("b2bua 1 3 IN IP4 192.0.2.1"));
    assert_eq!(sent_origin(&merged, 1), Some("b2bua 2 3 IN IP4 192.0.2.1"), "never an earlier one");
    assert_eq!(merged.b_legs[0].sdp_session.exchanges_opened, 2, "never lowered");
}

#[test]
fn the_next_rounds_request_raises_the_callers_remote_sequence() {
    let held = round(RoundStage::Open);
    let mut incoming = round(RoundStage::Acked);
    stage_reinvite_round(&mut incoming.b_legs[0], "z9hG4bK-round-2", RoundStage::Open);
    let next = incoming.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    next.peer_cseq = Some(next.peer_cseq.unwrap_or(0) + 1);
    let merged = merge_rounds(held, &incoming);
    assert!(invite_relay(callee(&merged)).is_some());
    assert_eq!(caller(&merged).peer_cseq, caller(&incoming).peer_cseq, "the next INVITE's CSeq");
}

#[test]
fn an_answer_the_round_owed_goes_with_its_end() {
    let mut held = round(RoundStage::Open);
    held.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog").owed_answer =
        Some(OwedAnswer::InPrack { rseq: RSeq(1), offer: b"v=0".to_vec() });
    let merged = merge_rounds(held, &round(RoundStage::Answered));
    assert_eq!(callee(&merged).owed_answer, None, "the PRACK's answer left with the round");
}

#[test]
fn an_answered_round_followed_by_the_next_round_ends() {
    let held = awaiting_the_callers_ack(round(RoundStage::Answered));
    let mut incoming = round(RoundStage::Acked);
    stage_reinvite_round(&mut incoming.b_legs[0], "z9hG4bK-round-2", RoundStage::Open);
    let merged = merge_rounds(held, &incoming);
    assert!(awaited_ack_cseq(callee(&merged)).is_none(), "the first round's ACK left");
    assert!(answered_2xx(caller(&merged)).is_none() && ladders(&merged) == 0);
    assert!(invite_relay(callee(&merged)).is_some());
}
