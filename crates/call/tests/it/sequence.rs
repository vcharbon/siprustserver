//! The in-dialog requests two copies of one call carry
//! (`call::helpers::sequence`): a body below the other on a dialog both hold
//! lacks a request the other sent or took there, the higher of the two is
//! what the next request continues from (RFC 3261 §12.2.1.1), and a relay one
//! copy awaits a final for is held by the other only as that relay.

use crate::common;
use call::helpers::{holds_relays_of, raise_sequences, sequence_behind};
use call::model::{Call, Dialog, Direction, PendingRequest};

fn caller_mut(call: &mut Call) -> &mut Dialog {
    call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog")
}

fn callee_mut(call: &mut Call) -> &mut Dialog {
    call.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog")
}

#[test]
fn a_body_below_on_either_sequence_lacks_a_request() {
    let held = common::representative_call();
    assert!(!sequence_behind(&held, &held), "a body is never below itself");

    let mut sent = held.clone();
    callee_mut(&mut sent).our_cseq += 1;
    assert!(sequence_behind(&held, &sent), "a request the other copy sent the callee");
    assert!(!sequence_behind(&sent, &held), "past it is not below it");

    let mut took = held.clone();
    let d = caller_mut(&mut took);
    d.peer_cseq = d.peer_cseq.map(|c| c + 1);
    assert!(sequence_behind(&held, &took), "a request the other copy took from the caller");
}

#[test]
fn a_dialog_only_one_body_holds_is_not_read() {
    let held = common::representative_call();
    let mut other = held.clone();
    callee_mut(&mut other).our_cseq += 1;
    other.b_legs.clear();
    assert!(!sequence_behind(&other, &held));
}

/// `call` relaying one more `method` toward the caller, awaiting its final.
fn relaying(mut call: Call, method: &str) -> Call {
    let d = caller_mut(&mut call);
    d.our_cseq += 1;
    d.inbound_pending_requests.push(PendingRequest {
        method: method.into(),
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
    call
}

#[test]
fn a_record_holds_the_relays_of_a_copy_only_with_each_of_them() {
    let held = common::representative_call();
    let copy = relaying(held.clone(), "INFO");
    assert!(!holds_relays_of(&held, &copy), "the copy's INFO is in no record but its own");
    assert!(holds_relays_of(&copy, &copy));
    assert!(holds_relays_of(&copy, &held), "a relay of the record's own is no relay of the copy");
    let raised = raise_sequences(held.clone(), &copy);
    assert!(!holds_relays_of(&raised, &copy), "a raised sequence is not the relay");
    assert!(
        holds_relays_of(&held, &relaying(held.clone(), "INVITE")),
        "rounds read their own rule"
    );
}

#[test]
fn the_higher_of_each_sequence_is_taken_and_nothing_else() {
    let mut held = common::representative_call();
    callee_mut(&mut held).our_cseq += 2;
    let mut incoming = common::representative_call();
    caller_mut(&mut incoming).our_cseq += 1;
    let d = caller_mut(&mut incoming);
    d.peer_cseq = d.peer_cseq.map(|c| c + 3);
    incoming.message_count = Some(99);

    let raised = raise_sequences(held.clone(), &incoming);

    let caller = |c: &Call| c.a_leg.dialogs.confirmed().cloned().expect("the caller's dialog");
    let callee = |c: &Call| c.b_legs[0].dialogs.confirmed().cloned().expect("the callee's dialog");
    assert_eq!(caller(&raised).our_cseq, caller(&incoming).our_cseq);
    assert_eq!(caller(&raised).peer_cseq, caller(&incoming).peer_cseq);
    assert_eq!(callee(&raised).our_cseq, callee(&held).our_cseq, "never lowered");
    assert_eq!(raised.message_count, held.message_count, "no other fact comes in");
}
