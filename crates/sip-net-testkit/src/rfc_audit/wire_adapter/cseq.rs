//! The live surfacing of the CSeq family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::taker(RuleId::CseqInDialogOrder, cseq_in_dialog_order),
    Live::taker(RuleId::ResponseCseqMatchesTransaction, response_cseq_matches_transaction),
    Live::taker(RuleId::AckCseqMatchesInvite, ack_cseq_matches_invite),
];

/// The `Call-ID=… from-tag=… to-tag=…` descriptor the CSeq findings name the
/// dialog by.
fn dialog_desc(call_id: &str, from_tag: &str, to_tag: &str) -> String {
    format!(
        "Call-ID={call_id} from-tag={from_tag} to-tag={}",
        if to_tag.is_empty() { "<none>" } else { to_tag },
    )
}

fn cseq_in_dialog_order(f: &rfc_rules::Finding, cid: &str) -> String {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::CseqReused {
            cseq,
            method,
            from_tag,
            to_tag,
            ..
        }) => format!(
            "in-dialog CSeq reused (RFC 3261 §12.2.1.1): {method} CSeq {cseq} reuses a \
             prior request's CSeq (a new in-dialog transaction must increment the dialog \
             CSeq by exactly one) on {dialog} — it rides a FRESH Via branch, so a real \
             UAS does not fold it away as a retransmission (§17.2.3): it is a new server \
             transaction whose sequence number did not advance and is rejected out of \
             order (§12.2.2 — 500, or an implementation-defined reject) (the test UA \
             answers it, hiding the bug)",
            dialog = dialog_desc(cid, from_tag, to_tag),
        ),
        Decision::Violated(rfc_rules::Evidence::CseqNotContiguous {
            cseq,
            prior_cseq,
            method,
            from_tag,
            to_tag,
            ..
        }) if cseq <= prior_cseq => format!(
            "in-dialog CSeq did not advance (RFC 3261 §12.2.1.1): {method} CSeq {cseq} \
             does not exceed the dialog-creating CSeq {prior_cseq} on {dialog}; the first \
             in-dialog request MUST increment the dialog CSeq by exactly one (expected \
             {expected}) — a value at or below the INVITE's own CSeq reuses or regresses \
             it, which a real UAS rejects (the test UA answers it, hiding the bug)",
            dialog = dialog_desc(cid, from_tag, to_tag),
            expected = prior_cseq + 1,
        ),
        Decision::Violated(rfc_rules::Evidence::CseqNotContiguous {
            cseq,
            prior_cseq,
            method,
            from_tag,
            to_tag,
            ..
        }) => format!(
            "in-dialog CSeq not contiguous (RFC 3261 §12.2.1.1): {method} CSeq {cseq} \
             skips ahead of CSeq {prior_cseq}; the UAC MUST increment the dialog CSeq by \
             exactly one (expected {expected}) on {dialog}",
            dialog = dialog_desc(cid, from_tag, to_tag),
            expected = prior_cseq + 1,
        ),
        _ => String::new(),
    }
}

fn response_cseq_matches_transaction(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseCseqUnmatched {
        status,
        response_cseq,
        response_method,
        txn_cseqs,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "response CSeq does not match its transaction (RFC 3261 §8.1.3.5): {status} \
         response carries CSeq {response_cseq} {response_method} on Via branch \
         {branch} (callId {cid}), but no request on that transaction had that CSeq \
         (it carried: {carried}) — a real UAC drops a response whose CSeq/method does \
         not match the request it sent (the test UA accepts it, hiding the bug)",
        carried = txn_cseqs.join(", "),
    )
}

fn ack_cseq_matches_invite(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AckCseqUnmatched {
        ack_cseq,
        invite_cseqs,
        from_tag,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "ACK CSeq does not match any INVITE (RFC 3261 §13.2.2.4): ACK CSeq {ack_cseq} on \
         Call-ID={cid} from-tag={from_tag} acknowledges no INVITE the stream sent (it \
         sent CSeq: {sent}) — an INVITE 2xx ACK reuses the INVITE's CSeq; a running \
         dialog CSeq advanced by an intervening PRACK/UPDATE is wrong — a real UAS cannot \
         match it to the INVITE server transaction",
        sent = invite_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
    )
}
