//! The compared fields of a call record and the closed exempt list
//! (`call::helpers::compared`, ADR-0055 §7): a write that changes only exempt
//! fields leaves the record's state as it was, and a meeting of two copies
//! merges the exempt fields of the other into the container.

use crate::common;
use call::helpers::{merge_exempt, same_state};
use call::model::{
    Call, CdrEvent, CdrEventType, DialogState, DialogUsage, EndingBy, HostPort, MessageDirection,
    MessageEntry, RequestRecord, SentRequest, TimerEntry, TimerKey, TimerType,
};

/// The representative call.
fn call() -> Call {
    common::representative_call()
}

/// A keepalive OPTIONS this stack sent on the caller's dialog at `cseq`.
fn keepalive(cseq: i64) -> RequestRecord {
    let sent = SentRequest {
        branch: format!("z9hG4bK-ka-{cseq}"),
        image: b"OPTIONS".to_vec(),
        destination: HostPort { host: "192.0.2.10".into(), port: 5060 },
    };
    RequestRecord::own("OPTIONS", cseq, sent, 32_000, DialogUsage::Session)
}

fn entry(seq: u32) -> MessageEntry {
    MessageEntry {
        seq,
        at_ms: i64::from(seq),
        direction: MessageDirection::Received,
        method: "OPTIONS".into(),
        cseq: seq,
        code: None,
        to_tag: None,
        decision_ordinal: 0,
        headers: Vec::new(),
        turn: seq,
    }
}

fn bye_event(leg: &str) -> CdrEvent {
    CdrEvent {
        event_type: CdrEventType::Bye,
        timestamp: 9,
        leg_id: leg.into(),
        status_code: None,
        reason: None,
        decision_ordinal: 0,
    }
}

/// A keepalive round leaves the record's state as it was: its request
/// record, the CSeq it took, its timers, the ring entries and the counters
/// are all exempt.
#[test]
fn a_keepalive_round_changes_no_compared_field() {
    let before = call();
    let mut after = before.clone();
    let d = after.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.our_cseq += 1;
    d.requests.push(keepalive(d.our_cseq));
    after.timers.push(TimerEntry {
        key: TimerKey::leg(TimerType::KeepaliveTimeout, "a".to_string()),
        generation: 9,
        fire_at: 32_000,
    });
    after.timer_generation = 9;
    after.a_leg.messages.entries.push(entry(7));
    after.message_seq = 7;
    after.message_turn += 1;
    after.message_count = Some(3);
    after.cdr_events.push(bye_event("a"));
    after.trace_id = Some("trace".into());
    assert!(same_state(&before, &after));
}

/// A request record other than a keepalive, or a dialog's state, is
/// compared.
#[test]
fn a_relay_or_a_dialog_state_is_compared() {
    let before = call();
    let mut relayed = before.clone();
    let d = relayed.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.requests.push(RequestRecord::relayed("INFO", 8006, 3));
    assert!(!same_state(&before, &relayed), "a relay");
    let mut ending = before.clone();
    let d = ending.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.state = DialogState::Ending(EndingBy::Bye);
    assert!(!same_state(&before, &ending), "a dialog ending");
}

/// A rung a quiet turn moves is exempt; the 2xx it repeats is compared.
#[test]
fn a_ladder_rung_is_exempt_and_its_emission_compared() {
    let before = call();
    let mut rung = before.clone();
    let d = rung.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    let answered = call::helpers::answered_2xx_mut(d).expect("the 2xx awaiting its ACK");
    assert!(answered.emission.advance(None).is_some(), "a rung left");
    assert!(same_state(&before, &rung), "the rung");
    let mut acked = before.clone();
    let d = acked.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.answered_invite = None;
    assert!(!same_state(&before, &acked), "the 2xx discharged");
}

/// The merge takes the container whole and the other copy's exempt fields:
/// the higher sequences, the union of the CDR events and of the rings.
#[test]
fn a_merge_keeps_the_container_and_raises_the_exempt_fields() {
    let mut container = call();
    container.cdr_events = vec![bye_event("a")];
    container.a_leg.messages.entries = vec![entry(1), entry(3)];
    let mut other = call();
    other.a_leg.dialogs.confirmed_mut().expect("the answered dialog").our_cseq = 9000;
    other.cdr_events = vec![bye_event("a"), bye_event("b-1")];
    other.a_leg.messages.entries = vec![entry(1), entry(2)];
    other.timer_generation = 40;
    other.callback_context = Some("the other copy's".into());

    let merged = merge_exempt(container.clone(), &other);
    let d = merged.a_leg.dialogs.confirmed().expect("the answered dialog");
    assert_eq!(d.our_cseq, 9000, "the higher sequence");
    assert_eq!(merged.cdr_events, vec![bye_event("a"), bye_event("b-1")]);
    let seqs: Vec<u32> = merged.a_leg.messages.entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(merged.timer_generation, 40);
    assert_eq!(merged.callback_context, container.callback_context, "a compared field");
}
