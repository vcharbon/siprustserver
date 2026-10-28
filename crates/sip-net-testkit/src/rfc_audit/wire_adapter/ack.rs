//! The live surfacing of the ACK family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter_only(RuleId::NoAckToDialogCreating2xx, no_ack_to_dialog_creating_2xx),
    Live::emitter_only(RuleId::Unacked2xxNotCleared, unacked_2xx_not_cleared),
    Live::emitter(RuleId::AckRequireSubsetOfInvite, ack_require_subset_of_invite)
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::AckPreservesInviteRoute, ack_preserves_invite_route)
        .skip(Skip::RelayLanes),
    // Gates: the harness UAC ACKs every non-2xx final (§17.1.1.3); a test modelling a peer that
    // never ACKs waives it.
    Live::taker_else_emitter(RuleId::UnackedInviteNon2xxFinal, unacked_invite_non_2xx_final),
];

/// The To tag a violated ACK-family finding names, off its evidence.
fn evidence_to_tag(f: &rfc_rules::Finding) -> &str {
    match &f.decision {
        Decision::Violated(
            rfc_rules::Evidence::NoAck { to_tag, .. }
            | rfc_rules::Evidence::Uncleared { to_tag, .. },
        ) => to_tag,
        _ => "",
    }
}

fn no_ack_to_dialog_creating_2xx(f: &rfc_rules::Finding, cid: &str) -> String {
    format!(
        "Took a 2xx confirming the dialog (callId {cid}, To-tag {tt}) to its own \
         INVITE (CSeq {cseq}) and never ACKed it; RFC 3261 §13.2.2.4 has the UAC \
         core ACK every 2xx received",
        tt = evidence_to_tag(f),
        cseq = f.cseq,
    )
}

fn unacked_2xx_not_cleared(f: &rfc_rules::Finding, cid: &str) -> String {
    format!(
        "Sent a 2xx to an INVITE (callId {cid}, To-tag {tt}) that was never ACKed \
         and the dialog was never BYE'd — the answered call leaks; RFC 3261 \
         §13.3.1.4 requires retransmitting the 2xx and, on no ACK, BYE-ing the \
         dialog",
        tt = evidence_to_tag(f),
    )
}

fn ack_require_subset_of_invite(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AckRequireNotSubset {
        ack_tags,
        invite_tags,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent ACK Require=[{}] not a subset of INVITE Require=[{}] (callId {cid}, branch \
         {branch}) — RFC 3261 §13.2.2.4 / RFC3261-MUST-035",
        ack_tags.join(", "),
        invite_tags.join(", "),
    )
}

fn ack_preserves_invite_route(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AckRouteDiverged {
        ack_routes,
        invite_routes,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent ACK Route values [{}] differ from INVITE Route values [{}] (callId {cid}, \
         branch {branch}) — RFC 3261 §17.1.1.3 / RFC3261-MUST-145",
        ack_routes.join(", "),
        invite_routes.join(", "),
    )
}

fn unacked_invite_non_2xx_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnackedReject { status, branch, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Took a non-2xx final {status} to an INVITE (callId {cid}, branch {branch}) and \
         never ACKed it to the UAS that sent it — the INVITE transaction never \
         completes and the reject retransmits to Timer H; RFC 3261 §17.1.1.3 makes \
         the ACK mandatory (hop-by-hop, so a proxy in the path owes the UAS its own \
         synthesized ACK)",
    )
}
