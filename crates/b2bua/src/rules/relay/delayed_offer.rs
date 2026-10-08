//! Whether an INVITE this stack sent toward a leg carried the offer — what
//! tells a reliable provisional's description apart as the OFFER (the INVITE
//! carried none, RFC 3264 §4) or the answer, and so whether the PRACK
//! acknowledging it owes the answer (RFC 3262 §5).

use call::Call;

use super::ack::parse_request;

/// Whether the INVITE of CSeq `invite_cseq` this stack sent on `leg_id` — the
/// leg's initial INVITE or a re-INVITE on one of its dialogs — carried a
/// session description. `true` where no such INVITE is cached: a provisional
/// whose INVITE this stack cannot read is never taken for an offer.
pub(crate) fn invite_carried_offer(call: &Call, leg_id: &str, invite_cseq: i64) -> bool {
    let Some(leg) = call::helpers::find_leg(call, leg_id) else { return true };
    std::iter::once(leg.pending_invite_txn.as_ref())
        .chain(leg.dialogs.iter().map(|d| d.ext.pending_invite_txn.as_ref()))
        .flatten()
        .filter_map(|h| parse_request(&h.original_invite))
        .find(|r| i64::from(r.cseq().seq()) == invite_cseq)
        .is_none_or(|r| r.sdp().is_some())
}
