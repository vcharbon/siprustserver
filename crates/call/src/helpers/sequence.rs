//! The in-dialog requests two copies of one call carry: the dialog sequences
//! they reached and the relays they hold. On one copy a dialog's own sequence
//! (the last CSeq this side sent) and its remote sequence (the last the peer
//! sent) only rise (RFC 3261 §12.2.1.1, §12.2.2), and every request either
//! copy relays or takes moves one of them. A body whose sequence stands below
//! the other copy's on a dialog both hold lacks a request the other copy sent
//! or took there, and the next request either copy sends on the dialog
//! continues from the higher of the two.

use crate::model::{Call, Dialog, Leg};

use super::leg::{every_leg, find_leg};
use super::round::holds_round_unfinished;

/// Does `held` hold every non-INVITE request `other` relays and awaits a
/// final for, on the same dialog at the same CSeq? A non-INVITE final follows
/// the Via to the node that relayed the request (RFC 3261 §18.2.2), and a
/// record that lacks the relay has nothing to relay the final to.
pub fn holds_relays_of(held: &Call, other: &Call) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = find_leg(held, &other_leg.leg_id);
        other_leg.dialogs.iter().all(|o| {
            let ours = held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag));
            o.inbound_pending_requests
                .iter()
                .filter(|p| !p.method.eq_ignore_ascii_case("INVITE"))
                .all(|p| {
                    ours.is_some_and(|d| {
                        d.inbound_pending_requests
                            .iter()
                            .any(|h| h.outbound_cseq == p.outbound_cseq && h.method == p.method)
                    })
                })
        })
    })
}

/// Does `incoming` stand below `held` on the own or the remote sequence of a
/// dialog both hold? Then `incoming` lacks a request `held` sent or took on
/// it.
pub fn sequence_behind(incoming: &Call, held: &Call) -> bool {
    every_leg(held).any(|held_leg| {
        let Some(incoming_leg) = find_leg(incoming, &held_leg.leg_id) else { return false };
        held_leg.dialogs.iter().any(|h| {
            incoming_leg
                .dialogs
                .by_tags(&h.our_tag, &h.peer_tag)
                .is_some_and(|i| i.our_cseq < h.our_cseq || i.peer_cseq < h.peer_cseq)
        })
    })
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

/// [`raise_sequences`] on the dialogs where `incoming` holds no re-INVITE
/// round unfinished: a record that refused `incoming` takes none of its
/// rounds, and a sequence raised past a round it lacks would read as the
/// round's end (`super::round::round_ahead`).
pub fn raise_settled_sequences(mut held: Call, incoming: &Call) -> Call {
    let settled = |t: &Dialog| !holds_round_unfinished(t);
    raise_leg_where(&mut held.a_leg, incoming, settled);
    for leg in held.b_legs.iter_mut() {
        raise_leg_where(leg, incoming, settled);
    }
    held
}

fn raise_leg(leg: &mut Leg, incoming: &Call) {
    raise_leg_where(leg, incoming, |_| true);
}

fn raise_leg_where(leg: &mut Leg, incoming: &Call, take: impl Fn(&Dialog) -> bool) {
    let Some(theirs) = find_leg(incoming, &leg.leg_id) else { return };
    for d in leg.dialogs.iter_mut() {
        if let Some(t) = theirs.dialogs.by_tags(&d.our_tag, &d.peer_tag).filter(|t| take(t)) {
            raise(d, t);
        }
    }
}

fn raise(d: &mut Dialog, theirs: &Dialog) {
    d.our_cseq = d.our_cseq.max(theirs.our_cseq);
    d.peer_cseq = d.peer_cseq.max(theirs.peer_cseq);
}
