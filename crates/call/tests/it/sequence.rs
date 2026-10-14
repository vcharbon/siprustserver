//! The in-dialog requests two copies of one call carry
//! (`call::helpers::sequence`): the higher of the two sequences is what the
//! next request continues from (RFC 3261 §12.2.1.1), and a relay one copy
//! awaits a final for is held by the other only as that relay.

use crate::common;
use call::helpers::{holds_relays_of, raise_sequences};
use call::model::{Call, Dialog, DialogState, Direction, PendingRequest, RetiredDialog};

/// The node that relayed the copy's requests.
const BY: &str = "w1";

fn caller_mut(call: &mut Call) -> &mut Dialog {
    call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog")
}

fn callee_mut(call: &mut Call) -> &mut Dialog {
    call.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog")
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
        relayed_by: BY.into(),
    });
    call
}

#[test]
fn a_record_holds_the_relays_of_a_copy_only_with_each_of_them() {
    let held = common::representative_call();
    let copy = relaying(held.clone(), "INFO");
    assert!(
        !holds_relays_of(Some(&held), &copy, BY),
        "the copy's INFO is in no record but its own"
    );
    assert!(holds_relays_of(Some(&copy), &copy, BY));
    assert!(
        holds_relays_of(Some(&copy), &held, BY),
        "a relay of the record's own is no relay of the copy"
    );
    let raised = raise_sequences(held.clone(), &copy);
    assert!(!holds_relays_of(Some(&raised), &copy, BY), "a raised sequence is not the relay");
    assert!(
        holds_relays_of(Some(&held), &relaying(held.clone(), "INVITE"), BY),
        "rounds read their own rule"
    );
}

/// Both copies relayed a request at one CSeq across a cut: the record's relay
/// answers another originator's request, and is not the copy's.
#[test]
fn a_relay_at_the_same_cseq_for_another_request_is_not_the_copys() {
    let held = common::representative_call();
    let copy = relaying(held.clone(), "INFO");
    let mut other = relaying(held.clone(), "INFO");
    caller_mut(&mut other).inbound_pending_requests.last_mut().expect("the INFO").inbound_cseq =
        2002;
    assert!(!holds_relays_of(Some(&other), &copy, BY), "another originator CSeq");
    let mut other = relaying(held.clone(), "MESSAGE");
    caller_mut(&mut other).inbound_pending_requests.last_mut().expect("the relay").method =
        "info".into();
    assert!(holds_relays_of(Some(&other), &copy, BY), "methods compare without case");
}

/// A retired dialog's relays still await their finals (RFC 3261 §8.1.3.3).
#[test]
fn a_retired_dialogs_open_relays_are_relays_of_the_copy() {
    let held = common::representative_call();
    let mut copy = relaying(held.clone(), "INFO");
    let d = caller_mut(&mut copy);
    let open_relays = std::mem::take(&mut d.inbound_pending_requests);
    d.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
    assert!(!holds_relays_of(Some(&held), &copy, BY));
    assert!(holds_relays_of(Some(&copy), &copy, BY));
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
