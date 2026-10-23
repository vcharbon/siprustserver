//! The books of the reliable provisionals a call received (RFC 3262): the one
//! record per provisional ([`Provisional`]), written as it arrives, then
//! updated as it is shown, as its ladder runs, and as the PRACK its responder
//! is owed leaves.
//!
//! A B2BUA is the UAS of whichever face a provisional leaves on, so the number
//! shown there is this stack's, never the responder's: toward the caller on an
//! initial INVITE or a caller re-INVITE, toward the callee on a callee
//! re-INVITE. ONE sequence per shown DIALOG, rising by exactly one within it:
//! RFC 3262 §4 as corrected by errata 4603/4604 keeps the sequence
//! "independently for each dialog ID", and §3 as corrected by errata 4600
//! makes each fork's numbering space independent of every other fork's.
//! Forks mirrored as distinct shown dialogs each get their own sequence, and
//! forks collapsed behind one shown tag share that tag's, which is what stops
//! two responder sequences from colliding in a single shown dialog.

use std::time::Duration;

use crate::model::{
    CSeq, Call, Ceased, Ladder, Provisional, ProvisionalId, RSeq, RequestRecord, ResponderAck,
    RetainedEmission, Shown,
};

/// What a reliable provisional arriving now is, read against the record
/// (RFC 3262 §3, §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The first of its dialog's sequence, or one higher than the highest
    /// recorded there: recorded, and handled.
    InOrder,
    /// A copy of one recorded already: the responder's §3 retransmission,
    /// which the UAC discards.
    Repeat,
    /// Its `RSeq` is not one higher than the highest recorded on its dialog:
    /// §4 has the UAC neither PRACK it nor process it further.
    OutOfOrder,
}

/// The record of the provisional `id` names.
pub fn provisional<'a>(call: &'a Call, id: &ProvisionalId) -> Option<&'a Provisional> {
    call.provisionals.iter().find(|p| p.id == *id)
}

fn provisional_mut<'a>(call: &'a mut Call, id: &ProvisionalId) -> Option<&'a mut Provisional> {
    call.provisionals.iter_mut().find(|p| p.id == *id)
}

/// Whether the record of `id` is a copy, out of order, or the next of its
/// responder dialog's sequence for that INVITE.
pub fn arrival(call: &Call, id: &ProvisionalId) -> Arrival {
    if provisional(call, id).is_some() {
        return Arrival::Repeat;
    }
    let highest = call
        .provisionals
        .iter()
        .filter(|p| {
            p.id.leg == id.leg
                && p.id.responder_tag == id.responder_tag
                && p.id.invite_cseq == id.invite_cseq
        })
        .map(|p| p.id.rseq)
        .max();
    match highest {
        Some(h) if id.rseq != h.next() => Arrival::OutOfOrder,
        _ => Arrival::InOrder,
    }
}

/// Record the reliable provisional `id` as it arrives: owed its PRACK, shown
/// to no one. `responder_sdp`: it carried a description. A recorded `id` is
/// left as it stands.
pub fn record_provisional(call: &mut Call, id: ProvisionalId, responder_sdp: bool) {
    if provisional(call, &id).is_some() {
        return;
    }
    call.provisionals.push(Provisional {
        id,
        responder_sdp,
        responder: ResponderAck::Pending,
        shown: None,
    });
}

/// Whether a provisional of `(leg, responder_tag, invite_cseq)` carrying a
/// description is recorded before `rseq`: only the first description of an
/// early dialog is its offer (RFC 3264 §4).
pub fn described_before(call: &Call, id: &ProvisionalId) -> bool {
    call.provisionals.iter().any(|p| {
        p.id.leg == id.leg
            && p.id.responder_tag == id.responder_tag
            && p.id.invite_cseq == id.invite_cseq
            && p.id.rseq < id.rseq
            && p.responder_sdp
    })
}

/// Whether the shown dialog `tag` has shown a reliable provisional yet: the
/// first number of a dialog is drawn at random (RFC 3262 §3), every later one
/// follows it.
pub fn starts_shown_sequence(call: &Call, tag: &str) -> bool {
    !call.provisionals.iter().any(|p| p.shown.as_ref().is_some_and(|s| s.tag == tag))
}

/// Show the provisional `id` in this stack's `tag` dialog, answering there the
/// INVITE `invite_cseq`, and return the number it is shown under: the one it
/// already holds, else the previous number of that dialog plus exactly one
/// (§4), the first of a dialog being `initial`. `carried_sdp`: it carries a
/// description toward that face. `None` for an unrecorded `id`.
pub fn show_provisional(
    call: &mut Call,
    id: &ProvisionalId,
    tag: &str,
    invite_cseq: CSeq,
    carried_sdp: bool,
    initial: RSeq,
) -> Option<RSeq> {
    if let Some(shown) = provisional(call, id)?.shown.as_ref() {
        return Some(shown.rseq);
    }
    let rseq = call
        .provisionals
        .iter()
        .filter_map(|p| p.shown.as_ref())
        .filter(|s| s.tag == tag)
        .map(|s| s.rseq)
        .max()
        .map_or(RSeq(initial.0.max(1)), RSeq::next);
    provisional_mut(call, id)?.shown = Some(Shown {
        tag: tag.to_string(),
        rseq,
        invite_cseq,
        carried_sdp,
        ladder: Ladder::Unsent,
    });
    Some(rseq)
}

/// The provisional shown as `rseq` in this stack's `tag` dialog.
pub fn shown_as<'a>(call: &'a Call, tag: &str, rseq: RSeq) -> Option<&'a Provisional> {
    call.provisionals
        .iter()
        .find(|p| p.shown.as_ref().is_some_and(|s| s.tag == tag && s.rseq == rseq))
}

fn shown_mut<'a>(call: &'a mut Call, tag: &str, rseq: RSeq) -> Option<&'a mut Shown> {
    call.provisionals
        .iter_mut()
        .filter_map(|p| p.shown.as_mut())
        .find(|s| s.tag == tag && s.rseq == rseq)
}

/// Put the emission of the provisional shown as `rseq` in `tag` on its §3
/// ladder. Only a ladder not yet sent takes one: a recalled number keeps the
/// ladder it anchors. Returns whether the ladder started, so the caller arms
/// its first rung exactly then.
pub fn start_shown_ladder(
    call: &mut Call,
    tag: &str,
    rseq: RSeq,
    emission: RetainedEmission,
) -> bool {
    match shown_mut(call, tag, rseq) {
        Some(s) if s.ladder == Ladder::Unsent => {
            s.ladder = Ladder::Repeating(emission);
            true
        }
        _ => false,
    }
}

/// The emission the ladder of `(tag, rseq)` repeats; `None` once it stopped.
pub fn shown_emission<'a>(call: &'a Call, tag: &str, rseq: RSeq) -> Option<&'a RetainedEmission> {
    match &shown_as(call, tag, rseq)?.shown.as_ref()?.ladder {
        Ladder::Repeating(e) => Some(e),
        _ => None,
    }
}

/// Step the ladder of `(tag, rseq)` onto its next rung (§3 doubling) and
/// return the wait before it, or `None` when it is over: not repeating, or
/// the next rung would land at or past `give_up` (the class bound where
/// `None`).
pub fn advance_shown_ladder(
    call: &mut Call,
    tag: &str,
    rseq: RSeq,
    give_up: Option<Duration>,
) -> Option<Duration> {
    match &mut shown_mut(call, tag, rseq)?.ladder {
        Ladder::Repeating(e) => e.advance(give_up),
        _ => None,
    }
}

/// Stop the ladder of `(tag, rseq)` short of its PRACK for `why`: the bytes
/// are never sent again. An acknowledged or already ceased ladder stands.
pub fn cease_shown_ladder(call: &mut Call, tag: &str, rseq: RSeq, why: Ceased) {
    if let Some(s) = shown_mut(call, tag, rseq) {
        if matches!(s.ladder, Ladder::Unsent | Ladder::Repeating(_)) {
            s.ladder = Ladder::Ceased(why);
        }
    }
}

/// Record the PRACK of the provisional shown as `rseq` in `tag` received from
/// the party it was shown to (RFC 3262 §3): it leaves the unacknowledged list
/// and its retransmissions stop. Returns whether this acknowledged it — a
/// provisional already acknowledged stays so, and returns `false`.
pub fn acknowledge_shown(call: &mut Call, tag: &str, rseq: RSeq) -> bool {
    match shown_mut(call, tag, rseq) {
        Some(s) if s.ladder != Ladder::Acknowledged => {
            s.ladder = Ladder::Acknowledged;
            true
        }
        _ => false,
    }
}

/// Note where the PRACK the responder of `id` is owed now stands.
pub fn note_responder_ack(call: &mut Call, id: &ProvisionalId, ack: ResponderAck) {
    if let Some(p) = provisional_mut(call, id) {
        p.responder = ack;
    }
}

/// The provisional a shown `rseq` in this stack's `tag` dialog stands for,
/// as its responder identifies it: what a relayed PRACK's `RAck` names once
/// translated. `None` when this stack showed no such number there.
pub fn responder_of<'a>(call: &'a Call, tag: &str, rseq: RSeq) -> Option<&'a ProvisionalId> {
    shown_as(call, tag, rseq).map(|p| &p.id)
}

/// Whether the provisional answering the relayed request `pending` may leave
/// toward its originator reliably (RFC 3262 §3): the request is an INVITE —
/// the one method the mechanism serves; §4 bars `Require: 100rel` from every
/// other and §7 Table 3 permits `RSeq` only in INVITE responses — and the
/// originator's OWN request offered `100rel`, without which "the UAS MUST NOT
/// send the provisional response reliably". A reliable provisional to a
/// request this refuses leaves unreliably and is shown to no one.
pub fn admits_reliable_provisional(pending: &RequestRecord) -> bool {
    pending.method.eq_ignore_ascii_case("INVITE") && pending.offered_100rel
}

/// The leg a reliable provisional shown in the `shown_tag` dialog was shown
/// ON — the leg whose dialog carries that tag as this stack's own. `None` for
/// a tag no dialog carries.
pub fn leg_shown<'a>(call: &'a Call, shown_tag: &str) -> Option<&'a str> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .find(|leg| leg.dialogs.iter().any(|d| d.our_tag == shown_tag))
        .map(|leg| leg.leg_id.as_str())
}
