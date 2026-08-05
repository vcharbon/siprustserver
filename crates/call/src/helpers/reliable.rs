//! The reliable-provisional sequence the back-to-back UA shows the caller
//! (RFC 3262): minting the a-facing `RSeq` and translating it back to the
//! b-leg's own.
//!
//! `RSeq` is per-INVITE-transaction sequencing, exactly like `CSeq` — so the
//! number the caller sees is this stack's, not the callee's, and every early
//! dialog of one a-leg transaction shares a single ladder.

use crate::model::{Call, ReliableProvisional};

/// The a-facing `RSeq` standing for `(b_leg_id, b_rseq)`, minting one if this
/// b-leg provisional has not been relayed before. Idempotent: a retransmitted
/// reliable provisional yields the SAME number, so the caller sees the
/// retransmission it is (RFC 3262 §3), not a new provisional. A fresh number is
/// the previous one plus exactly one (§3); the first is `initial`, which the
/// caller draws at random (§7.1) and which is ignored once the ladder started.
pub fn assign_a_rseq(
    mut call: Call,
    b_leg_id: &str,
    b_rseq: i64,
    initial: i64,
) -> (Call, i64) {
    if let Some(known) =
        call.reliable_provisionals.iter().find(|r| r.b_leg_id == b_leg_id && r.b_rseq == b_rseq)
    {
        let a_rseq = known.a_rseq;
        return (call, a_rseq);
    }
    let a_rseq = match call.reliable_provisionals.iter().map(|r| r.a_rseq).max() {
        Some(highest) => highest + 1,
        None => initial.max(1),
    };
    call.reliable_provisionals.push(ReliableProvisional {
        a_rseq,
        b_leg_id: b_leg_id.to_string(),
        b_rseq,
    });
    (call, a_rseq)
}

/// The b-leg `RSeq` an a-facing `a_rseq` stands for on `b_leg_id` — what a
/// relayed PRACK's `RAck` names once translated. `None` when this stack minted
/// no such provisional toward that leg, and the caller then relays what it
/// received (the callee answers 481 if it means nothing there).
pub fn b_rseq_for(call: &Call, b_leg_id: &str, a_rseq: i64) -> Option<i64> {
    call.reliable_provisionals
        .iter()
        .find(|r| r.b_leg_id == b_leg_id && r.a_rseq == a_rseq)
        .map(|r| r.b_rseq)
}
