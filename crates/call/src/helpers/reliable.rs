//! The reliable-provisional sequence the back-to-back UA shows the caller
//! (RFC 3262): minting the a-facing `RSeq` and translating it back to the
//! b-leg's own.
//!
//! The number the caller sees is this stack's, not the callee's. ONE ladder per
//! a-facing EARLY DIALOG, rising by exactly one within it: RFC 3262 §4 as
//! corrected by errata 4603/4604 keeps the sequence "independently for each
//! dialog ID", and §3 as corrected by errata 4600 makes each fork's numbering
//! space independent of every other fork's. A ladder spanning several a-facing
//! dialogs would show a conformant caller a gap inside one of them.
//!
//! The dialog is the unit whichever shape the relay takes: forks mirrored as
//! distinct a-facing dialogs each get their own ladder, and forks collapsed
//! behind one a-facing tag share that tag's ladder — which is what stops two
//! callee sequences from colliding in a single caller dialog.

use crate::model::{Call, ReliableProvisional};

/// The a-facing `RSeq` standing for `(b_leg_id, b_tag, b_cseq, b_rseq)` in the
/// `a_tag` early dialog, minting one if this b-leg provisional has not been
/// relayed before. Idempotent: a retransmitted reliable provisional yields the
/// SAME number, so the caller sees the retransmission it is (RFC 3262 §3), not
/// a new provisional. A fresh number is the previous one in THIS dialog plus
/// exactly one (§4); the first in a dialog is `initial`, drawn at random (§3)
/// and ignored once that dialog's ladder began.
#[allow(clippy::too_many_arguments)]
pub fn assign_a_rseq(
    mut call: Call,
    a_tag: &str,
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
    });
    (call, a_rseq)
}

/// The b-leg and `RSeq` an a-facing `a_rseq` stands for in the `a_tag` early
/// dialog — what a relayed PRACK's `RAck` names once translated. The caller
/// PRACKs within the dialog she was shown the number in, so that dialog is the
/// key: an `a_rseq` is unique only there. `None` when this stack minted no such
/// provisional, and the caller then relays what it received (the callee answers
/// 481 if it means nothing there).
pub fn b_rseq_for<'a>(call: &'a Call, a_tag: &str, a_rseq: i64) -> Option<(&'a str, i64)> {
    call.reliable_provisionals
        .iter()
        .find(|r| r.a_tag == a_tag && r.a_rseq == a_rseq)
        .map(|r| (r.b_leg_id.as_str(), r.b_rseq))
}

/// Whether the `a_tag` early dialog has shown the caller a reliable provisional
/// yet — the caller of [`assign_a_rseq`] draws a random initial sequence only
/// for a dialog that has not.
pub fn starts_reliable_ladder(call: &Call, a_tag: &str) -> bool {
    !call.reliable_provisionals.iter().any(|r| r.a_tag == a_tag)
}
