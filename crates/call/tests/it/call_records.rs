//! The call's own records as the helpers write them: the decision log and
//! the ordinal each ring entry and CDR event carries, the turn counter, the
//! termination and its cut.

use crate::common;
use call::helpers::*;
use call::model::*;
use common::representative_call;

/// Every mark bumps the ordinal and carries it; the ring append and the CDR
/// append stamp whatever count stands at that instant, so an entry written
/// between two decisions keeps the first one's ordinal after the second lands.
#[test]
fn marks_number_the_decisions_and_stamp_what_follows() {
    let mut call = representative_call();
    call.decision_log.clear();
    call.decision_ordinal = 0;
    call.b_legs[0].messages = MessageRing::default();
    let entry = || MessageEntry {
        seq: 0,
        at_ms: 5,
        direction: MessageDirection::Relayed,
        method: "INVITE".into(),
        cseq: 1,
        code: None,
        to_tag: None,
        from_tag: None,
        decision_ordinal: 99,
        headers: Vec::new(),
        turn: 0,
    };
    let cdr = || CdrEvent {
        event_type: CdrEventType::InviteSent,
        timestamp: 5,
        leg_id: "b-1".into(),
        status_code: None,
        reason: None,
        decision_ordinal: 99,
    };

    record_message(&mut call, "b-1", 8, entry());
    add_cdr_event(&mut call, cdr());
    mark_decision(&mut call, 10, DecisionKind::Route, Some("a".into()), Some("first".into()));
    record_message(&mut call, "b-1", 8, entry());
    add_cdr_event(&mut call, cdr());
    mark_decision(&mut call, 20, DecisionKind::FailoverRoute, Some("b-1".into()), None);
    record_message(&mut call, "b-1", 8, entry());
    add_cdr_event(&mut call, cdr());

    assert_eq!(call.decision_ordinal, 2);
    assert_eq!(
        call.decision_log,
        vec![
            DecisionMark {
                ordinal: 1,
                at_ms: 10,
                kind: DecisionKind::Route,
                leg_id: Some("a".into()),
                label: Some("first".into()),
            },
            DecisionMark {
                ordinal: 2,
                at_ms: 20,
                kind: DecisionKind::FailoverRoute,
                leg_id: Some("b-1".into()),
                label: None,
            },
        ]
    );
    let ring: Vec<u32> =
        call.b_legs[0].messages.entries.iter().map(|e| e.decision_ordinal).collect();
    assert_eq!(ring, vec![0, 1, 2]);
    let events: Vec<u32> =
        call.cdr_events.iter().rev().take(3).rev().map(|e| e.decision_ordinal).collect();
    assert_eq!(events, vec![0, 1, 2]);
}

/// Every entry appended carries the counter's turn, whatever the entry held;
/// the seal moves the counter past every turn, one that appended nothing
/// included: one turn, one number.
#[test]
fn a_turn_numbers_its_entries_and_the_seal_moves_past_it() {
    let mut call = representative_call();
    call.a_leg.messages = MessageRing::default();
    call.b_legs[0].messages = MessageRing::default();
    call.message_seq = 0;
    call.message_turn = 1;
    let entry = || MessageEntry {
        seq: 0,
        at_ms: 5,
        direction: MessageDirection::Received,
        method: "INVITE".into(),
        cseq: 1,
        code: None,
        to_tag: None,
        from_tag: None,
        decision_ordinal: 0,
        headers: Vec::new(),
        turn: 99,
    };

    record_message(&mut call, "a", 8, entry());
    record_message(&mut call, "b-1", 8, entry());
    seal_turn(&mut call);
    assert_eq!(call.message_turn, 2, "past the turn that recorded two entries");
    seal_turn(&mut call);
    assert_eq!(call.message_turn, 3, "a turn that recorded nothing takes a number too");
    record_message(&mut call, "a", 8, entry());
    seal_turn(&mut call);
    assert_eq!(call.message_turn, 4);

    let turns = |leg: &Leg| leg.messages.entries.iter().map(|e| e.turn).collect::<Vec<u32>>();
    assert_eq!(turns(&call.a_leg), vec![1, 3]);
    assert_eq!(turns(&call.b_legs[0]), vec![1]);
}

/// The first termination's record stands: a second write under another
/// cause changes nothing, and the cut is taken once — the ring's last seq at
/// the seal, never moved by a later seal.
#[test]
fn the_first_termination_is_recorded_once_and_cut_once() {
    let mut call = representative_call();
    call.termination = None;
    call.message_seq = 4;
    seal_termination_seq(&mut call);
    assert_eq!(call.termination, None, "nothing to cut");

    record_termination(&mut call, 10, TerminationCause::RemoteCancel, Some("a".into()));
    record_termination(&mut call, 20, TerminationCause::Supervisor, None);
    assert_eq!(
        call.termination,
        Some(Termination {
            at_ms: 10,
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
            last_seq: 0,
        })
    );
    seal_termination_seq(&mut call);
    assert_eq!(call.termination.as_ref().unwrap().last_seq, 4, "cut at the seal");
    call.message_seq = 7;
    seal_termination_seq(&mut call);
    assert_eq!(call.termination.as_ref().unwrap().last_seq, 4, "a later seal moves nothing");
}
