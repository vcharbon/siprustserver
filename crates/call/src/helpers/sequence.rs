//! The in-dialog requests two copies of one call carry: the dialog sequences
//! they reached and the request records they hold. On one copy a dialog's own sequence
//! (the last CSeq this side sent) and its remote sequence (the last the peer
//! sent) only rise (RFC 3261 §12.2.1.1, §12.2.2), and every request either
//! copy relays or takes moves one of them; the next request either copy sends
//! on a dialog continues from the higher of the two.

use crate::model::{Call, Leg, RelayOwner, RequestRecord};

use super::leg::{every_leg, find_leg};
use super::request_records::records_of;

/// Does `held` — a record, or none — hold every non-INVITE request record
/// the worker `by` keeps open in `other`, as the same record on the same
/// dialog — its origin, the CSeq it went out with, the method, and the CSeq
/// and Call-ID of the request it received — a retired dialog's open records
/// included? A non-INVITE final follows the Via to the node that sent the
/// request (RFC 3261 §18.2.2): only `by` sees the final of a relay or an own
/// request it sent, and only `by` holds the server transaction of a request
/// it holds for a later final; a record that lacks it settles none of these.
/// A record another worker made — another ordinal, or a former boot of
/// `by`'s — returns to that worker. INVITE relays are rounds, read by
/// `super::round`.
pub fn holds_records_of(held: Option<&Call>, other: &Call, by: &RelayOwner) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = held.and_then(|h| find_leg(h, &other_leg.leg_id));
        other_leg.dialogs.iter().all(|o| {
            let ours = held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag));
            records_of(o)
                .iter()
                .filter(|p| !p.method.eq_ignore_ascii_case("INVITE") && p.relayed_by == *by)
                .all(|p| ours.is_some_and(|d| records_of(d).iter().any(|h| same_record(h, p))))
        })
    })
}

fn same_record(a: &RequestRecord, b: &RequestRecord) -> bool {
    a.origin == b.origin
        && a.outbound_cseq == b.outbound_cseq
        && a.inbound_cseq == b.inbound_cseq
        && a.method.eq_ignore_ascii_case(&b.method)
        && a.source_call_id == b.source_call_id
}

/// `held` with, on every dialog both hold, the higher of the two own
/// sequences and of the two remote sequences, and nothing else of
/// `incoming`.
pub fn raise_sequences(mut held: Call, incoming: &Call) -> Call {
    raise_leg(&mut held.a_leg, incoming);
    for leg in held.b_legs.iter_mut() {
        raise_leg(leg, incoming);
    }
    held
}

fn raise_leg(leg: &mut Leg, incoming: &Call) {
    let Some(theirs) = find_leg(incoming, &leg.leg_id) else { return };
    for d in leg.dialogs.iter_mut() {
        if let Some(t) = theirs.dialogs.by_tags(&d.our_tag, &d.peer_tag) {
            d.our_cseq = d.our_cseq.max(t.our_cseq);
            d.peer_cseq = d.peer_cseq.max(t.peer_cseq);
        }
    }
}
