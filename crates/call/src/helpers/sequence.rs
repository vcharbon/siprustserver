//! The in-dialog requests two copies of one call carry: the dialog sequences
//! they reached and the request records they hold. On one copy a dialog's own sequence
//! (the last CSeq this side sent) and its remote sequence (the last the peer
//! sent) only rise (RFC 3261 §12.2.1.1, §12.2.2), and every request either
//! copy relays or takes moves one of them; the next request either copy sends
//! on a dialog continues from the higher of the two.

use crate::model::{Call, Dialog, Leg, RelayOwner, RequestRecord};

use super::invite_records::{invite_image, invite_relay};
use super::leg::{every_leg, find_leg};
use super::request_records::records_of;

/// Does `held` — a record, or none — hold every request record the worker
/// `by` keeps open in `other`, a retired dialog's open records included? A
/// final follows the Via to the node that sent the request (RFC 3261
/// §18.2.2): only `by` sees the final of a relay or an own request it sent,
/// and only `by` holds the server transaction of a request it holds for a
/// later final; a record that lacks it settles none of these. A record
/// another worker made — another ordinal, or a former boot of `by`'s —
/// returns to that worker.
///
/// A non-INVITE record is held as the same record on the same dialog: its
/// origin, the CSeq it went out with, the method, and the CSeq and Call-ID of
/// the request it received. An INVITE relay awaiting its final is held by the
/// same dialog's last INVITE when it is the same transaction or a later one;
/// an INVITE whose 2xx is taken awaits no final (its ACK follows the route
/// set, not the node).
pub fn holds_records_of(held: Option<&Call>, other: &Call, by: &RelayOwner) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = held.and_then(|h| find_leg(h, &other_leg.leg_id));
        other_leg.dialogs.iter().all(|o| {
            let ours = held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag));
            let records = records_of(o)
                .iter()
                .filter(|p| !p.method.eq_ignore_ascii_case("INVITE") && p.relayed_by == *by)
                .all(|p| ours.is_some_and(|d| records_of(d).iter().any(|h| same_record(h, p))));
            records && holds_invite_relay(held_leg.zip(ours), other_leg, o, by)
        })
    })
}

/// Whether `ours` — the same dialog in the other record, on its leg — holds
/// the INVITE relay `by` made that `theirs`, a dialog of `their_leg`, holds
/// awaiting its final: the same transaction, or a later INVITE.
fn holds_invite_relay(
    ours: Option<(&Leg, &Dialog)>,
    their_leg: &Leg,
    theirs: &Dialog,
    by: &RelayOwner,
) -> bool {
    let Some(relay) = invite_relay(theirs).filter(|r| r.relayed_by == *by) else {
        return true;
    };
    let Some(branch) = invite_image(their_leg, relay).map(|s| s.branch.as_str()) else {
        return true;
    };
    let held = ours.and_then(|(leg, d)| {
        let record = d.sent_invite.as_ref()?;
        Some((invite_image(leg, record)?.branch.as_str(), record.outbound_cseq))
    });
    held.is_some_and(|(b, cseq)| b == branch || cseq > relay.outbound_cseq)
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

/// `ending` with every dialog `held` holds and `ending` lacks, on the leg both
/// hold, kept as `held` holds it: a record that ends the call names every
/// dialog either copy opened (ADR-0055 §7).
pub fn keep_dialogs(mut ending: Call, held: &Call) -> Call {
    let keep = |leg: &mut Leg| {
        let Some(mine) = find_leg(held, &leg.leg_id) else { return };
        for d in mine.dialogs.iter() {
            if leg.dialogs.by_tags(&d.our_tag, &d.peer_tag).is_none() {
                leg.dialogs.insert(d.clone());
            }
        }
    };
    keep(&mut ending.a_leg);
    ending.b_legs.iter_mut().for_each(keep);
    ending
}
