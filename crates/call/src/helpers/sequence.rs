//! The in-dialog requests two copies of one call carry: the dialog sequences
//! they reached and the relays they hold. On one copy a dialog's own sequence
//! (the last CSeq this side sent) and its remote sequence (the last the peer
//! sent) only rise (RFC 3261 §12.2.1.1, §12.2.2), and every request either
//! copy relays or takes moves one of them; the next request either copy sends
//! on a dialog continues from the higher of the two.

use crate::model::{Call, Dialog, Leg, PendingRequest};

use super::leg::{every_leg, find_leg};

/// Does `held` hold every non-INVITE request `other` relays and awaits a
/// final for, as the same relay on the same dialog — the CSeq it went out
/// with, the method, and the originator's CSeq and Call-ID it answers — a
/// retired dialog's open relays included? A non-INVITE final follows the Via
/// to the node that relayed the request (RFC 3261 §18.2.2), and a record that
/// lacks the relay has nothing to relay the final to. INVITE relays are
/// rounds, read by `super::round`.
pub fn holds_relays_of(held: &Call, other: &Call) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = find_leg(held, &other_leg.leg_id);
        other_leg.dialogs.iter().all(|o| {
            let ours = held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag));
            relays(o)
                .filter(|p| !p.method.eq_ignore_ascii_case("INVITE"))
                .all(|p| ours.is_some_and(|d| relays(d).any(|h| same_relay(h, p))))
        })
    })
}

/// The relays a dialog awaits finals for, open or held by its retirement.
fn relays(d: &Dialog) -> impl Iterator<Item = &PendingRequest> {
    let retired = d.retired().map(|r| r.open_relays.as_slice()).unwrap_or_default();
    d.inbound_pending_requests.iter().chain(retired.iter())
}

fn same_relay(a: &PendingRequest, b: &PendingRequest) -> bool {
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
