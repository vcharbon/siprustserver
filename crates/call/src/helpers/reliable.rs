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

use crate::model::{
    Call, PendingRequest, PrackedProvisional, ReliableProvisional, RetainedEmission,
};

/// The shown `RSeq` standing for the responder's `(b_leg_id, b_tag, b_cseq,
/// b_rseq)` in the `a_tag` dialog — the dialog the provisional is relayed
/// into, keyed by this stack's own tag there — minting one if this provisional
/// has not been relayed before. Idempotent: a retransmitted reliable
/// provisional yields the SAME number, so the peer sees the retransmission it
/// is (RFC 3262 §3), not a new provisional. A fresh number is the previous one
/// in THIS dialog plus exactly one (§4); the first in a dialog is `initial`,
/// drawn at random (§3) and ignored once that dialog's ladder began. `a_cseq`
/// is the CSeq number of the INVITE on the shown face, recorded so the PRACK
/// can be matched on the whole `RAck` (§7.2); `carried_sdp` whether the
/// provisional carries a description there, `responder_sdp` whether the
/// responder's did.
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
    carried_sdp: bool,
    responder_sdp: bool,
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
        a_cseq,
        carried_sdp,
        responder_sdp,
        responder_offer: None,
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
    let Some(r) = call.reliable_provisionals.iter_mut().find(|r| {
        r.a_tag == a_tag && r.a_rseq == a_rseq && !r.acknowledged && r.emission.is_none()
    }) else {
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
pub fn advance_reliable_ladder(
    mut call: Call,
    a_tag: &str,
    a_rseq: i64,
) -> (Call, Option<Duration>) {
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
    for r in
        call.reliable_provisionals.iter_mut().filter(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
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
    for r in
        call.reliable_provisionals.iter_mut().filter(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
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
/// — the caller PRACKs exactly when this returns `true`. `responder_sdp`:
/// the provisional carried a description.
pub fn record_pracked_provisional(
    mut call: Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
    responder_sdp: bool,
) -> (Call, bool) {
    if pracked_provisional(&call, leg_id, remote_tag, invite_cseq, rseq) {
        return (call, false);
    }
    call.pracked_provisionals.push(PrackedProvisional {
        leg_id: leg_id.to_string(),
        remote_tag: remote_tag.to_string(),
        invite_cseq,
        rseq,
        responder_sdp,
        branch: String::new(),
    });
    (call, true)
}

/// Note `branch` as the PRACK client transaction this stack sent for the
/// responder's `(leg_id, remote_tag, invite_cseq, rseq)` provisional.
pub fn note_own_prack_branch(
    mut call: Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
    branch: &str,
) -> Call {
    if let Some(p) = call.pracked_provisionals.iter_mut().find(|p| {
        p.leg_id == leg_id
            && p.remote_tag == remote_tag
            && p.invite_cseq == invite_cseq
            && p.rseq == rseq
    }) {
        p.branch = branch.to_string();
    }
    call
}

/// Whether `branch` is a PRACK client transaction this stack originated
/// itself — not one it relayed — so its failure denies that acknowledgement
/// only (RFC 3261 §14.1 by analogy: one transaction, never the dialog).
pub fn own_prack_branch(call: &Call, branch: &str) -> bool {
    !branch.is_empty() && call.pracked_provisionals.iter().any(|p| p.branch == branch)
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
        p.leg_id == leg_id
            && p.remote_tag == remote_tag
            && p.invite_cseq == invite_cseq
            && p.rseq == rseq
    })
}

/// Whether the responder's `(leg_id, remote_tag, invite_cseq, rseq)` reliable
/// provisional has been acknowledged toward it (RFC 3262 §4): PRACKed by this
/// stack itself, or relayed and PRACKed by the party it was shown to.
pub fn provisional_acknowledged(
    call: &Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
) -> bool {
    pracked_provisional(call, leg_id, remote_tag, invite_cseq, rseq)
        || call.reliable_provisionals.iter().any(|r| {
            r.acknowledged
                && r.b_leg_id == leg_id
                && r.b_tag == remote_tag
                && r.b_cseq == invite_cseq
                && r.b_rseq == rseq
        })
}

/// Whether the responder's reliable provisional `rseq` on the `(leg_id,
/// remote_tag)` early dialog of the INVITE `invite_cseq` is the next in its
/// sequence (RFC 3262 §4): the dialog's first, or one higher than the highest
/// this stack has taken there — relayed or PRACKed. One out of order is
/// neither PRACKed nor processed further.
pub fn rseq_in_order(
    call: &Call,
    leg_id: &str,
    remote_tag: &str,
    invite_cseq: i64,
    rseq: i64,
) -> bool {
    let pracked = call
        .pracked_provisionals
        .iter()
        .filter(|p| {
            p.leg_id == leg_id && p.remote_tag == remote_tag && p.invite_cseq == invite_cseq
        })
        .map(|p| p.rseq);
    let relayed = call
        .reliable_provisionals
        .iter()
        .filter(|r| r.b_leg_id == leg_id && r.b_tag == remote_tag && r.b_cseq == invite_cseq)
        .map(|r| r.b_rseq);
    pracked.chain(relayed).max().is_none_or(|highest| rseq == highest + 1)
}

/// A relayed reliable provisional this stack still owes a PRACK as it CANCELs
/// the INVITE it answers ([`unacknowledged_relayed_provisionals`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwedPrack {
    /// The responder's tag on the early dialog the PRACK rides.
    pub b_tag: String,
    /// The `CSeq` number of the responder-facing INVITE.
    pub invite_cseq: i64,
    /// The `RSeq` the responder stated.
    pub rseq: i64,
    /// The provisional carried a description.
    pub responder_sdp: bool,
    /// The OFFER it carried, which the PRACK answers (RFC 3262 §5).
    pub offer: Option<Vec<u8>>,
}

/// The reliable provisionals `leg_id`'s responder sent on the INVITE
/// `invite_cseq` that were relayed and still await a PRACK from anyone — the
/// party shown them has not acknowledged them, nor has this stack — in the
/// order they were relayed. `None` reads every INVITE of the leg: a leg still
/// Trying or Early has only its initial one.
pub fn unacknowledged_relayed_provisionals(
    call: &Call,
    leg_id: &str,
    invite_cseq: Option<i64>,
) -> Vec<OwedPrack> {
    call.reliable_provisionals
        .iter()
        .filter(|r| r.b_leg_id == leg_id && invite_cseq.is_none_or(|c| c == r.b_cseq))
        .filter(|r| !provisional_acknowledged(call, leg_id, &r.b_tag, r.b_cseq, r.b_rseq))
        .map(|r| OwedPrack {
            b_tag: r.b_tag.clone(),
            invite_cseq: r.b_cseq,
            rseq: r.b_rseq,
            responder_sdp: r.responder_sdp,
            offer: r.responder_offer.clone(),
        })
        .collect()
}

/// Keep `offer` as the OFFER the responder's relayed provisional `(b_leg_id,
/// b_tag, b_cseq, b_rseq)` carried (RFC 3264 §4: its INVITE carried none).
pub fn note_responder_offer(
    mut call: Call,
    b_leg_id: &str,
    b_tag: &str,
    b_cseq: i64,
    b_rseq: i64,
    offer: &[u8],
) -> Call {
    if let Some(r) = call.reliable_provisionals.iter_mut().find(|r| {
        r.b_leg_id == b_leg_id && r.b_tag == b_tag && r.b_cseq == b_cseq && r.b_rseq == b_rseq
    }) {
        r.responder_offer = Some(offer.to_vec());
    }
    call
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
    let shown =
        call.reliable_provisionals.iter().find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)?;
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
/// dialog, and `cseq` that entry's recorded a-facing INVITE. The books state
/// WAS NEVER SHOWN, narrower than §4's "unacknowledged": a repeat PRACK for a
/// rung already acknowledged still matches, and relays. The books are
/// complete: every reliable provisional shown on either face is recorded
/// where it leaves, so an absent entry IS the negative.
pub fn unacknowledgeable_rack(
    call: &Call,
    source_leg_id: &str,
    a_tag: &str,
    rack: RAckTokens,
) -> bool {
    owns_rseq_numbering(call, source_leg_id) && !acknowledges_recorded(call, a_tag, rack)
}

/// Whether a PRACK in the `a_tag` dialog naming `rack` acknowledges a
/// provisional this stack already PRACKed toward its responder itself — as it
/// CANCELled the INVITE it answered. Relaying it would hand the responder a
/// second PRACK for one `RSeq`, which it answers 481 (RFC 3262 §3); this face
/// owes the 200 instead.
pub fn rack_pracked_here(call: &Call, a_tag: &str, rack: RAckTokens) -> bool {
    rack.names_invite
        && call.reliable_provisionals.iter().any(|r| {
            r.a_tag == a_tag
                && r.a_rseq == rack.rseq
                && r.a_cseq == rack.cseq
                && pracked_provisional(call, &r.b_leg_id, &r.b_tag, r.b_cseq, r.b_rseq)
        })
}

/// Whether a PRACK in the `a_tag` dialog naming `rack` is answered 200 by this
/// face rather than relayed (RFC 3262 §3): it names a provisional this stack
/// showed there, and either this stack already PRACKed its responder
/// ([`rack_pracked_here`]) or the call is ending, its responder's leg with it.
pub fn prack_answered_here(call: &Call, a_tag: &str, rack: RAckTokens) -> bool {
    rack_pracked_here(call, a_tag, rack)
        || (call.state != crate::CallModelState::Active && acknowledges_recorded(call, a_tag, rack))
}

/// Whether `rack` names a reliable provisional recorded in the `a_tag` early
/// dialog on all three §7.2 tokens.
fn acknowledges_recorded(call: &Call, a_tag: &str, rack: RAckTokens) -> bool {
    rack.names_invite
        && call
            .reliable_provisionals
            .iter()
            .any(|r| r.a_tag == a_tag && r.a_rseq == rack.rseq && r.a_cseq == rack.cseq)
}

/// Whether the `a_tag` early dialog has shown the caller a reliable provisional
/// yet — the caller of [`assign_a_rseq`] draws a random initial sequence only
/// for a dialog that has not.
pub fn starts_reliable_ladder(call: &Call, a_tag: &str) -> bool {
    !call.reliable_provisionals.iter().any(|r| r.a_tag == a_tag)
}

/// One dialog's reliable provisionals this stack showed and the party shown
/// them never PRACKed: the dialog's `Call-ID`, this stack's tag in it, and
/// each provisional's `(RSeq, CSeq-num)` — the §7.2 tokens a PRACK naming it
/// carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnackedShown {
    pub call_id: String,
    pub tag: String,
    pub racks: Vec<(i64, i64)>,
}

/// The reliable provisionals shown on either face of `call` that are still
/// unacknowledged by the party shown them, grouped per shown dialog. A tag no
/// dialog carries is an a-facing fork tag, the a-leg's ([`leg_shown`]).
pub fn unacknowledged_shown(call: &Call) -> Vec<UnackedShown> {
    let mut out: Vec<UnackedShown> = Vec::new();
    for r in call.reliable_provisionals.iter().filter(|r| !r.acknowledged) {
        let call_id = leg_shown(call, &r.a_tag)
            .and_then(|id| crate::helpers::find_leg(call, id))
            .unwrap_or(&call.a_leg)
            .call_id
            .clone();
        match out.iter_mut().find(|u| u.call_id == call_id && u.tag == r.a_tag) {
            Some(u) => u.racks.push((r.a_rseq, r.a_cseq)),
            None => out.push(UnackedShown {
                call_id,
                tag: r.a_tag.clone(),
                racks: vec![(r.a_rseq, r.a_cseq)],
            }),
        }
    }
    out
}
