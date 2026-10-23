//! The in-dialog requests two copies of one call carry: the dialog sequences
//! they reached and the relays they hold. On one copy a dialog's own sequence
//! (the last CSeq this side sent) and its remote sequence (the last the peer
//! sent) only rise (RFC 3261 §12.2.1.1, §12.2.2), and every request either
//! copy relays or takes moves one of them; the next request either copy sends
//! on a dialog continues from the higher of the two.

use crate::model::{Call, Dialog, Leg, RelayOwner, RequestRecord};

use super::leg::{every_leg, find_leg};

/// Does `held` — a record, or none — hold every non-INVITE request the worker
/// `by` relayed in `other` and awaits a final for, as the same relay on the
/// same dialog — the CSeq it went out with, the method, and the originator's
/// CSeq and Call-ID it answers — a retired dialog's open relays included? A
/// non-INVITE final follows the Via to the node that relayed the request
/// (RFC 3261 §18.2.2): only `by` sees the final of a relay it made, and a
/// record that lacks the relay has nothing to relay that final to; a relay
/// another worker made — another ordinal, or a former boot of `by`'s — returns
/// to that worker. INVITE relays are rounds, read by
/// `super::round`.
pub fn holds_relays_of(held: Option<&Call>, other: &Call, by: &RelayOwner) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = held.and_then(|h| find_leg(h, &other_leg.leg_id));
        other_leg.dialogs.iter().all(|o| {
            let ours = held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag));
            relays(o)
                .filter(|p| !p.method.eq_ignore_ascii_case("INVITE") && p.relayed_by == *by)
                .all(|p| ours.is_some_and(|d| relays(d).any(|h| same_relay(h, p))))
        })
    })
}

/// The relays a dialog awaits finals for, open or held by its retirement.
fn relays(d: &Dialog) -> impl Iterator<Item = &RequestRecord> {
    super::request_records::records_of(d).iter().filter(|p| p.is_relay())
}

fn same_relay(a: &RequestRecord, b: &RequestRecord) -> bool {
    a.outbound_cseq == b.outbound_cseq
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
