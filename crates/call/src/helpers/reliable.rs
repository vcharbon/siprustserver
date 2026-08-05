//! The reliable-provisional sequence the back-to-back UA shows the caller
//! (RFC 3262): minting the a-facing `RSeq` and translating it back to the
//! b-leg's own.
//!
//! The number the caller sees is this stack's, not the callee's. ONE ladder
//! serves the whole call, rising by exactly one per provisional: RFC 3262 §4
//! reads the sequence per REQUEST ("another reliable provisional response to
//! the same request"), so every early dialog of one a-leg INVITE shares it and
//! a forked second answer is the next rung, never a gap. A UAC that instead
//! tracks per early dialog sees gaps under interleaved forks — the §3/§4
//! forking ambiguity, resolved here in favour of §4's literal text.

use crate::model::{Call, ReliableProvisional};

/// The a-facing `RSeq` standing for `(b_leg_id, b_cseq, b_rseq)`, minting one
/// if this b-leg provisional has not been relayed before. Idempotent: a
/// retransmitted reliable provisional yields the SAME number, so the caller
/// sees the retransmission it is (RFC 3262 §3), not a new provisional — and
/// `b_cseq` is what keeps a re-INVITE's own restarted sequence from colliding
/// with the initial INVITE's. A fresh number is the previous one plus exactly
/// one (§3); the first is `initial`, which the caller draws at random (§7.1)
/// and which is ignored once the ladder started.
pub fn assign_a_rseq(
    mut call: Call,
    b_leg_id: &str,
    b_cseq: i64,
    b_rseq: i64,
    initial: i64,
) -> (Call, i64) {
    if let Some(known) = call
        .reliable_provisionals
        .iter()
        .find(|r| r.b_leg_id == b_leg_id && r.b_cseq == b_cseq && r.b_rseq == b_rseq)
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
        b_cseq,
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
