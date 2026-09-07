//! The reliable-provisional sequence the back-to-back UA shows the party it
//! relays a provisional TOWARD (RFC 3262): minting the shown `RSeq` and
//! translating it back to the responder's own.
//!
//! A B2BUA is the UAS of whichever face a provisional leaves on, so the number
//! shown there is this stack's, never the responder's — toward the caller on
//! an initial INVITE or a caller re-INVITE, toward the callee on a callee
//! re-INVITE. The books name the shown side `a_*` and the responding side
//! `b_*` after the common case; the key is the SHOWN dialog's tag, this
//! stack's own on either face, so the two faces never share a ladder. ONE
//! ladder per shown DIALOG, rising by exactly one within it: RFC 3262 §4 as
//! corrected by errata 4603/4604 keeps the sequence "independently for each
//! dialog ID", and §3 as corrected by errata 4600 makes each fork's numbering
//! space independent of every other fork's. A ladder spanning several shown
//! dialogs would show a conformant peer a gap inside one of them.
//!
//! The dialog is the unit whichever shape the relay takes: forks mirrored as
//! distinct a-facing dialogs each get their own ladder, and forks collapsed
//! behind one a-facing tag share that tag's ladder — which is what stops two
//! callee sequences from colliding in a single caller dialog.

use std::time::Duration;

use crate::model::{Call, PendingRequest, PrackedProvisional, ReliableProvisional, RetainedEmission};

/// The shown `RSeq` standing for the responder's `(b_leg_id, b_tag, b_cseq,
/// b_rseq)` in the `a_tag` dialog — the dialog the provisional is relayed
/// into, keyed by this stack's own tag there — minting one if this provisional
/// has not been relayed before. Idempotent: a retransmitted reliable
/// provisional yields the SAME number, so the peer sees the retransmission it
/// is (RFC 3262 §3), not a new provisional. A fresh number is the previous one
/// in THIS dialog plus exactly one (§4); the first in a dialog is `initial`,
/// drawn at random (§3) and ignored once that dialog's ladder began. `a_cseq`
/// is the CSeq number of the INVITE on the shown face, recorded so the PRACK
/// can be matched on the whole `RAck` (§7.2).
#[allow(clippy::too_many_arguments)]
pub fn assign_a_rseq(
    mut call: Call,
    a_tag: &str,
    a_cseq: i64,
    b_leg_id: &str,
    b_tag: &str,
    b_cseq: i64,
    b_rseq: i64,
    initial: i64,
) -> (Call, i64) {
    if let Some(known) = call.reliable_provisionals.iter().find(|r| {
        r.b_leg_id == b_leg_id && r.b_tag == b_tag && r.b_cseq == b_cseq && r.b_rseq == b_rseq
    }) {
        let a_rseq = known.a_rseq;
        return (call, a_rseq);
    }
    let a_rseq = match call
        .reliable_provisionals
        .iter()
        .filter(|r| r.a_tag == a_tag)
        .map(|r| r.a_rseq)
        .max()
    {
        Some(highest) => highest + 1,
        None => initial.max(1),
    };
    call.reliable_provisionals.push(ReliableProvisional {
        a_tag: a_tag.to_string(),
        a_rseq,
        b_leg_id: b_leg_id.to_string(),
        b_tag: b_tag.to_string(),
        b_cseq,
        b_rseq,
        acknowledged: false,
        emission: None,
        a_cseq: Some(a_cseq),
    });
    (call, a_rseq)
}

/// Retain `emission` — the provisional shown as `a_rseq` in the `a_tag` early
/// dialog as it left, on its RFC 3262 §3 ladder. First emission wins: a
/// recalled number keeps the ladder it already anchors, and an acknowledged
/// entry retains nothing (its retransmissions have ceased). Returns whether
/// this call recorded a NEW emission — the caller arms the first rung exactly
/// then.
pub fn record_reliable_provisional_emission(
    mut call: Call,
    a_tag: &str,
    a_rseq: i64,
    emission: RetainedEmission,
) -> (Call, bool) {
    let Some(r) = call
        .reliable_provisionals
        .iter_mut()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq && !r.acknowledged && r.emission.is_none())
    else {
        return (call, false);
    };
    r.emission = Some(emission);
    (call, true)
}

/// The live §3 ladder emission of `(a_tag, a_rseq)` — the datagram a rung
/// re-sends and where the ladder stands. `None` once the PRACK retired it or
/// the ladder ceased.
pub fn reliable_provisional_emission<'a>(
    call: &'a Call,
    a_tag: &str,
    a_rseq: i64,
) -> Option<&'a RetainedEmission> {
    call.reliable_provisionals
        .iter()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq && !r.acknowledged)
        .and_then(|r| r.emission.as_ref())
}

/// Step the `(a_tag, a_rseq)` ladder onto its next rung (RFC 3262 §3
/// doubling, bounded at 64·T1) and return the wait before it, or `None` when
/// the ladder is over — no live emission, or the next rung would land at or
/// past the bound. The caller arms the rung's timer, or ceases.
pub fn advance_reliable_ladder(mut call: Call, a_tag: &str, a_rseq: i64) -> (Call, Option<Duration>) {
    let next = call
        .reliable_provisionals
        .iter_mut()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq && !r.acknowledged)
        .and_then(|r| r.emission.as_mut())
        .and_then(|e| e.advance(None));
    (call, next)
}

/// Drop the `(a_tag, a_rseq)` ladder's retained emission: the ladder is over —
/// PRACKed, ceased at 64·T1, or cancelled with the setup — so the bytes can
/// never be sent again and stop riding the replicated body.
pub fn clear_reliable_provisional_emission(mut call: Call, a_tag: &str, a_rseq: i64) -> Call {
    for r in call
        .reliable_provisionals
        .iter_mut()
        .filter(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
    {
        r.emission = None;
    }
    call
}

/// Mark the provisional shown as `a_rseq` in the `a_tag` early dialog
/// acknowledged: the caller's matching PRACK is received, so RFC 3262 §3
/// removes it from the unacknowledged list and its a-facing retransmissions
/// cease. Keyed the way the PRACK names it — per a-facing early dialog — so
/// under forking one fork's PRACK never retires another fork's provisional.
pub fn retire_a_rseq(mut call: Call, a_tag: &str, a_rseq: i64) -> Call {
    for r in call
        .reliable_provisionals
        .iter_mut()
        .filter(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
    {
        r.acknowledged = true;
        r.emission = None;
    }
    call
}

/// Drop every retained §3 ladder emission on the call: a final response toward
/// the caller (or the setup's teardown) ends every early-dialog provisional at
/// once, whichever fork it belongs to — no rung may follow the final
/// (RFC 3261 §17.2.1; RFC 3262 §3).
pub fn clear_all_reliable_provisional_emissions(mut call: Call) -> Call {
    for r in call.reliable_provisionals.iter_mut() {
        r.emission = None;
    }
    call
}

/// Whether the b-leg provisional `(b_leg_id, b_tag, b_cseq, b_rseq)` has
/// already been relayed toward the caller — a matching entry exists, PRACKed
/// or not ([`assign_a_rseq`] records exactly one per provisional). A repeat of
/// it is a b-face retransmission the UAC discards outright (RFC 3262 §4 —
/// unconditionally, whatever the caller has done yet); the caller's own
/// further copies are the a face's §3 ladder to send, on its own clock.
pub fn reliable_provisional_relayed(
    call: &Call,
    b_leg_id: &str,
    b_tag: &str,
    b_cseq: i64,
    b_rseq: i64,
) -> bool {
    call.reliable_provisionals.iter().any(|r| {
        r.b_leg_id == b_leg_id && r.b_tag == b_tag && r.b_cseq == b_cseq && r.b_rseq == b_rseq
    })
}

/// Record that this stack PRACKed the responder's `(leg_id, remote_tag,
/// invite_cseq, rseq)` provisional itself. Returns whether this is the FIRST
/// acknowledgement of it: a repeat of an acknowledged provisional is the
/// responder's §3 retransmission, and RFC 3262 §4 has the receiver discard it
/// — the caller PRACKs exactly when this returns `true`.
pub fn record_pracked_provisional(
    mut call: Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
) -> (Call, bool) {
    if pracked_provisional(&call, leg_id, remote_tag, invite_cseq, rseq) {
        return (call, false);
    }
    call.pracked_provisionals.push(PrackedProvisional {
        leg_id: leg_id.to_string(),
        remote_tag: remote_tag.to_string(),
        invite_cseq,
        rseq,
    });
    (call, true)
}

/// Whether this stack has already PRACKed the responder's `(leg_id,
/// remote_tag, invite_cseq, rseq)` provisional itself, so a copy of it
/// arriving now is a retransmission to discard (RFC 3262 §4) — not a
/// provisional to relay, acknowledge or account again.
pub fn pracked_provisional(
    call: &Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
) -> bool {
    call.pracked_provisionals.iter().any(|p| {
        p.leg_id == leg_id && p.remote_tag == remote_tag && p.invite_cseq == invite_cseq && p.rseq == rseq
    })
}

/// The relayed INVITE transaction, still pending toward its target, that the
/// provisional shown as `a_rseq` in the `a_tag` dialog answers: the target leg
/// and the CSeq the request carries there. `None` for the initial INVITE (no
/// pending relay: the setup is the call), for a transaction already resolved
/// or CANCELled, and for a number this stack never minted. RFC 3262 §3's
/// give-up rejects the ORIGINAL REQUEST, so this is what tells an in-dialog
/// reject — one transaction, the dialog untouched (RFC 3261 §14.1) — from a
/// setup teardown.
pub fn pending_invite_answered_by(call: &Call, a_tag: &str, a_rseq: i64) -> Option<(String, i64)> {
    let shown = call
        .reliable_provisionals
        .iter()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)?;
    let leg = crate::helpers::find_leg(call, &shown.b_leg_id)?;
    leg.dialogs
        .iter()
        .find_map(|d| crate::helpers::find_pending_request(d, shown.b_cseq))
        .filter(|p| p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled)
        .map(|p| (shown.b_leg_id.clone(), p.outbound_cseq))
}

/// The responder's leg and `RSeq` a shown `a_rseq` stands for in the `a_tag`
/// dialog — what a relayed PRACK's `RAck` names once translated. The peer
/// PRACKs within the dialog it was shown the number in, so that dialog is the
/// key: an `a_rseq` is unique only there. `None` when this stack minted no such
/// provisional.
pub fn b_rseq_for<'a>(call: &'a Call, a_tag: &str, a_rseq: i64) -> Option<(&'a str, i64)> {
    call.reliable_provisionals
        .iter()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
        .map(|r| (r.b_leg_id.as_str(), r.b_rseq))
}

/// The three tokens a PRACK's `RAck` carries (RFC 3262 §7.2), as read off the
/// request: the response-num, the CSeq-num, and whether the method token is
/// `INVITE` — the only method a reliable provisional ever answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RAckTokens {
    pub rseq: i64,
    pub cseq: i64,
    pub names_invite: bool,
}

/// Whether `leg_id` is a face whose reliable-provisional numbering is this
/// stack's own — every leg of the call: a reliable provisional leaves toward a
/// face only under a number [`assign_a_rseq`] minted, whichever end sent the
/// INVITE it answers. On each face this stack is the UAS a PRACK is addressed
/// to, so it owes the answer to a `RAck` naming nothing (RFC 3262 §4) or to a
/// PRACK carrying none (RFC 3261 §21.4.1) — whether or not the dialog has
/// shown a reliable provisional: a masking profile strips the `RSeq` before
/// the caller sees it, and a PRACK into such a dialog names nothing all the
/// same. Answers on the leg named, so an unknown leg owns nothing.
pub fn owns_rseq_numbering(call: &Call, leg_id: &str) -> bool {
    crate::helpers::find_leg(call, leg_id).is_some()
}

/// Whether the provisional answering the relayed request `pending` may leave
/// toward its originator reliably (RFC 3262 §3): the request is an INVITE —
/// the one method the mechanism serves; §4 bars `Require: 100rel` from every
/// other and §7 Table 3 permits `RSeq` only in INVITE responses — and the
/// originator's OWN request offered `100rel`, without which "the UAS MUST NOT
/// send the provisional response reliably". A reliable provisional to a
/// request this refuses is stripped of its reliability instead of numbered.
pub fn admits_reliable_provisional(pending: &PendingRequest) -> bool {
    pending.method.eq_ignore_ascii_case("INVITE") && pending.offered_100rel
}

/// The leg a reliable provisional shown in the `shown_tag` dialog was shown
/// ON — the leg whose dialog carries that tag as this stack's own. `None` for
/// a tag no dialog carries: an a-facing fork tag that is only in the tag map,
/// which is the a-leg's by construction.
pub fn leg_shown<'a>(call: &'a Call, shown_tag: &str) -> Option<&'a str> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .find(|leg| leg.dialogs.iter().any(|d| d.sip.local_tag == shown_tag))
        .map(|leg| leg.leg_id.as_str())
}

/// Whether a PRACK arriving on `source_leg_id` in the `a_tag` dialog, naming
/// `rack`, PROVABLY acknowledges nothing this stack showed there — RFC 3262 §4
/// answers that 481, and this face owes it rather than the far party: the
/// responder numbers independently (§3, errata 4600), so relaying an `RAck`
/// this stack cannot translate would let it acknowledge, on a coincidental
/// match in its own sequence space, a provisional the PRACKing party was never
/// shown. Only on a face whose numbering is this stack's
/// ([`owns_rseq_numbering`]).
///
/// A match needs every §7.2 token: the method `INVITE`, `rseq` an entry of this
/// dialog, and `cseq` that entry's recorded a-facing INVITE. An entry that
/// recorded no CSeq cannot disprove the token and admits it. The books state
/// WAS NEVER SHOWN, narrower than §4's "unacknowledged": a repeat PRACK for a
/// rung already acknowledged still matches, and relays. The books are
/// complete: every reliable provisional shown on either face is recorded
/// where it leaves, so an absent entry IS the negative.
pub fn unacknowledgeable_rack(call: &Call, source_leg_id: &str, a_tag: &str, rack: RAckTokens) -> bool {
    owns_rseq_numbering(call, source_leg_id) && !acknowledges_recorded(call, a_tag, rack)
}

/// Whether `rack` names a reliable provisional recorded in the `a_tag` early
/// dialog on all three §7.2 tokens.
fn acknowledges_recorded(call: &Call, a_tag: &str, rack: RAckTokens) -> bool {
    rack.names_invite
        && call.reliable_provisionals.iter().any(|r| {
            r.a_tag == a_tag && r.a_rseq == rack.rseq && r.a_cseq.is_none_or(|c| c == rack.cseq)
        })
}

/// Whether the `a_tag` early dialog has shown the caller a reliable provisional
/// yet — the caller of [`assign_a_rseq`] draws a random initial sequence only
/// for a dialog that has not.
pub fn starts_reliable_ladder(call: &Call, a_tag: &str) -> bool {
    !call.reliable_provisionals.iter().any(|r| r.a_tag == a_tag)
}
