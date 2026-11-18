//! The compared fields of a call record and the closed exempt list
//! (`call::helpers::compared`, ADR-0055 §7): a write that changes only exempt
//! fields leaves the record's state as it was, and a meeting of two copies
//! merges the exempt fields of the other into the container.

use crate::common;
use call::helpers::{merge_exempt, same_state};
use call::model::{
    AckAhead, Call, CdrEvent, CdrEventType, DialogState, DialogUsage, EndingBy, HostPort,
    MessageDirection, MessageEntry, ReAckedCopy, Repeated, RequestRecord, RetainedEmission,
    SentRequest, TimerEntry, TimerKey, TimerType,
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
    RequestRecord {
        author: Some(call::KEEPALIVE_AUTHOR.into()),
        ..RequestRecord::own("OPTIONS", cseq, sent, 32_000, DialogUsage::Session)
    }
}

/// An OPTIONS a service rule `author` sent on its own is the service's
/// request, compared like any other: only the keepalive's is exempt.
#[test]
fn a_service_s_own_options_is_compared() {
    let before = call();
    let mut after = before.clone();
    let d = after.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.our_cseq += 1;
    let mut probe = keepalive(d.our_cseq);
    probe.author = Some("service-probe".into());
    d.requests.push(probe);
    assert!(!same_state(&before, &after));
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
/// record, the CSeq it took, the keepalive re-armed, the ring entries and
/// the counters are all exempt.
#[test]
fn a_keepalive_round_changes_no_compared_field() {
    let before = call();
    let mut after = before.clone();
    let d = after.a_leg.dialogs.confirmed_mut().expect("the answered dialog");
    d.our_cseq += 1;
    d.requests.push(keepalive(d.our_cseq));
    after.timers.push(TimerEntry {
        key: TimerKey::call(TimerType::Keepalive),
        generation: 9,
        fire_at: 300_000,
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

/// A retired dialog's records are compared as a live dialog's: its keepalive
/// is exempt, any other record compared.
#[test]
fn a_retired_dialogs_records_are_compared_as_a_live_dialogs() {
    let mut before = call();
    before.a_leg.dialogs.confirmed_mut().expect("the answered dialog").state = DialogState::Retired;
    let mut probed = before.clone();
    let d = probed.a_leg.dialogs.iter_mut().find(|d| d.is_retired()).expect("retired");
    d.our_cseq += 1;
    d.requests.push(keepalive(d.our_cseq));
    assert!(same_state(&before, &probed), "its keepalive");
    let mut relayed = before.clone();
    let d = relayed.a_leg.dialogs.iter_mut().find(|d| d.is_retired()).expect("retired");
    d.requests.push(RequestRecord::relayed("INFO", 8006, 3));
    assert!(!same_state(&before, &relayed), "a relay");
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

/// The copies of a taken 2xx counted on the INVITE's record are exempt, and a
/// merge keeps the higher count of the INVITE both copies hold: a copy that
/// counted more copies of the 2xx keeps its count.
#[test]
fn a_2xx_copy_count_is_exempt_and_merges_to_the_higher() {
    let copies = |call: &mut Call, n: u32| {
        let d = call.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
        call::helpers::sent_exchange_mut(d).expect("the INVITE's exchange").copies_received = n;
    };
    let read = |call: &Call| {
        let d = call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
        call::helpers::sent_exchange(d).expect("the INVITE's exchange").copies_received
    };
    let before = call();
    let mut counted = before.clone();
    copies(&mut counted, 4);
    assert!(same_state(&before, &counted), "the count");

    let mut container = call();
    copies(&mut container, 2);
    let mut other = call();
    copies(&mut other, 7);
    assert_eq!(read(&merge_exempt(container.clone(), &other)), 7, "the other's higher count");
    assert_eq!(read(&merge_exempt(other.clone(), &container)), 7, "the container's own");

    let d = other.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
    d.sent_invite.as_mut().expect("the INVITE's record").outbound_cseq += 1;
    assert_eq!(
        read(&merge_exempt(container, &other)),
        2,
        "another INVITE's count is not this one's"
    );
}

/// The last re-ACKed copy of a taken 2xx is exempt, and a merge keeps the
/// later of the two copies' own: a copy that re-ACKed later keeps its instant.
#[test]
fn a_re_acked_copy_is_exempt_and_merges_to_the_later() {
    let re_acked = |call: &mut Call, copy: u32, at_ms: i64| {
        let d = call.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange_mut(d).expect("the INVITE's exchange");
        x.re_acked_copy = Some(ReAckedCopy { copy, at_ms });
    };
    let read = |call: &Call| {
        let d = call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
        call::helpers::sent_exchange(d).expect("the INVITE's exchange").re_acked_copy
    };
    let before = call();
    let mut acked = before.clone();
    re_acked(&mut acked, 3, 1_500);
    assert!(same_state(&before, &acked), "the re-ACKed copy");

    let mut container = call();
    re_acked(&mut container, 11, 39_000);
    let mut other = call();
    re_acked(&mut other, 9, 31_000);
    let later = Some(ReAckedCopy { copy: 11, at_ms: 39_000 });
    assert_eq!(read(&merge_exempt(container.clone(), &other)), later, "the container's later");
    assert_eq!(read(&merge_exempt(other.clone(), &container)), later, "the other's later");
    assert_eq!(read(&merge_exempt(before.clone(), &other)), read(&other), "one copy's only");

    let mut late_low = call();
    re_acked(&mut late_low, 3, 40_000);
    let mut early_high = call();
    re_acked(&mut early_high, 11, 39_000);
    let latest = Some(ReAckedCopy { copy: 3, at_ms: 40_000 });
    assert_eq!(read(&merge_exempt(late_low.clone(), &early_high)), latest, "the later arrival");
    assert_eq!(read(&merge_exempt(early_high, &late_low)), latest, "over the higher copy");
}

/// The copies of the 2xx before the INVITE a dialog last sent, counted on the
/// ACK that INVITE carries ahead, are exempt as that INVITE's own are, and a
/// merge keeps the higher count and the later re-ACKed copy; the ACK itself
/// is compared.
#[test]
fn the_count_carried_ahead_is_exempt_and_merges_to_the_higher() {
    let ahead = |call: &mut Call, copies: u32, at_ms: i64| {
        let d = call.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange_mut(d).expect("the INVITE's exchange");
        x.ack_ahead = Some(AckAhead {
            ack: RetainedEmission::on_trigger(
                b"ACK sip:bob@192.0.2.20 SIP/2.0\r\n\r\n".to_vec(),
                ("192.0.2.20".into(), 5060),
                Repeated::request("ACK"),
            ),
            copies_received: copies,
            re_acked_copy: Some(ReAckedCopy { copy: copies, at_ms }),
        });
    };
    let read = |call: &Call| {
        let d = call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange(d).expect("the INVITE's exchange");
        x.ack_ahead.as_ref().map(|a| (a.copies_received, a.re_acked_copy))
    };
    let mut before = call();
    ahead(&mut before, 0, 0);
    let mut counted = before.clone();
    ahead(&mut counted, 4, 1_500);
    assert!(same_state(&before, &counted), "the count carried ahead");
    let mut dropped = before.clone();
    let d = dropped.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
    call::helpers::sent_exchange_mut(d).expect("the INVITE's exchange").ack_ahead = None;
    assert!(!same_state(&before, &dropped), "the ACK carried ahead");

    let mut container = call();
    ahead(&mut container, 2, 1_000);
    let mut other = call();
    ahead(&mut other, 7, 9_000);
    let higher = Some((7, Some(ReAckedCopy { copy: 7, at_ms: 9_000 })));
    assert_eq!(read(&merge_exempt(container.clone(), &other)), higher, "the other's higher");
}

/// A copy that sent the dialog's next INVITE counts the 2xx before it on the
/// ACK it carries ahead; a copy still on that earlier INVITE counts the same
/// 2xx on its own exchange. A merge keeps the higher count of that one 2xx
/// and its later re-ACKed copy, whichever of the two is the container.
#[test]
fn the_count_carried_ahead_merges_with_the_count_on_the_invite_it_left() {
    let earlier = |copies: u32, at_ms: i64| {
        let mut call = call();
        let d = call.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange_mut(d).expect("the INVITE's exchange");
        x.copies_received = copies;
        x.re_acked_copy = Some(ReAckedCopy { copy: copies, at_ms });
        call
    };
    let next = |copies: u32, at_ms: i64| {
        let mut call = call();
        let d = call.b_legs[0].dialogs.confirmed_mut().expect("the confirmed dialog");
        let record = d.sent_invite.as_mut().expect("the INVITE's record");
        record.outbound_cseq += 1;
        let x = record.invite.as_mut().expect("the INVITE's exchange");
        x.copies_received = 0;
        x.re_acked_copy = None;
        x.ack_ahead = Some(AckAhead {
            ack: RetainedEmission::on_trigger(
                b"ACK sip:bob@192.0.2.20 SIP/2.0\r\n\r\n".to_vec(),
                ("192.0.2.20".into(), 5060),
                Repeated::request("ACK"),
            ),
            copies_received: copies,
            re_acked_copy: Some(ReAckedCopy { copy: copies, at_ms }),
        });
        call
    };
    let ahead = |call: &Call| {
        let d = call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange(d).expect("the INVITE's exchange");
        x.ack_ahead.as_ref().map(|a| (a.copies_received, a.re_acked_copy))
    };
    let own = |call: &Call| {
        let d = call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
        let x = call::helpers::sent_exchange(d).expect("the INVITE's exchange");
        (x.copies_received, x.re_acked_copy)
    };

    assert_eq!(
        ahead(&merge_exempt(next(2, 1_000), &earlier(7, 9_000))),
        Some((7, Some(ReAckedCopy { copy: 7, at_ms: 9_000 }))),
        "the count ahead rises to the earlier INVITE's own"
    );
    assert_eq!(
        own(&merge_exempt(earlier(7, 9_000), &next(9, 12_000))),
        (9, Some(ReAckedCopy { copy: 9, at_ms: 12_000 })),
        "the earlier INVITE's own count rises to the one carried ahead"
    );
    assert_eq!(
        ahead(&merge_exempt(next(9, 12_000), &earlier(7, 9_000))),
        Some((9, Some(ReAckedCopy { copy: 9, at_ms: 12_000 }))),
        "a higher count ahead stays"
    );
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

/// `event` at `timestamp`.
fn at(mut event: CdrEvent, timestamp: i64) -> CdrEvent {
    event.timestamp = timestamp;
    event
}

/// An event both copies hold, read through two clock frames a few
/// milliseconds apart, is one event; one the other copy alone holds is
/// added, even a near twin of an event both hold (ADR-0055 §9).
#[test]
fn the_union_counts_an_event_both_copies_hold_once_across_clock_frames() {
    let mut container = call();
    container.cdr_events = vec![at(bye_event("a"), 1_000)];
    container.a_leg.messages.entries = vec![entry(1)];
    let mut other = call();
    other.cdr_events = vec![at(bye_event("a"), 1_900), at(bye_event("a"), 1_003)];
    let mut skewed = entry(1);
    skewed.at_ms += 4;
    other.a_leg.messages.entries = vec![skewed];

    let merged = merge_exempt(container, &other);
    let instants: Vec<i64> = merged.cdr_events.iter().map(|e| e.timestamp).collect();
    assert_eq!(instants, [1_000, 1_900], "the shared event once, the other's own added");
    assert_eq!(merged.a_leg.messages.entries.len(), 1, "the shared entry once");
}

/// Two forks' provisionals on one leg, both copies holding both, the other
/// copy's read half a second off: each pairs with its own, none is doubled.
#[test]
fn provisionals_of_two_forks_pair_in_instant_order() {
    let ringing = |timestamp| CdrEvent {
        event_type: CdrEventType::Provisional,
        timestamp,
        leg_id: "b-1".into(),
        status_code: Some(180),
        reason: None,
        decision_ordinal: 0,
    };
    let mut container = call();
    container.cdr_events = vec![ringing(0), ringing(900)];
    let mut other = call();
    other.cdr_events = vec![ringing(500), ringing(1_400)];

    let merged = merge_exempt(container, &other);
    let instants: Vec<i64> = merged.cdr_events.iter().map(|e| e.timestamp).collect();
    assert_eq!(instants, [0, 900], "two provisionals, as each copy holds");
}
