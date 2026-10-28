//! The live surfacing of the re-INVITE family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    // No relay skip: a face passing two racing re-INVITEs on is the UAS of two server transactions
    // it never serialised.
    Live::emitter_only(RuleId::ConcurrentReInvite500Or491, concurrent_re_invite_500_or_491),
    // No relay skip: the ordered dialog key spares a lane relaying both parties' crossing re-
    // INVITEs (glare).
    Live::emitter_only(
        RuleId::NoReInviteWhileInviteInProgress,
        no_re_invite_while_invite_in_progress,
    ),
    // No relay skip: a relaying lane opens its own client transaction on its own branch.
    Live::emitter_only(RuleId::FailedReinviteTearsDownDialog, failed_reinvite_tears_down_dialog),
];

fn concurrent_re_invite_500_or_491(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ConcurrentReInvite {
        branch,
        pending_invite_branch,
        answered_status,
        retry_after,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Took a concurrent re-INVITE (callId {cid}, branch {branch}) while a prior \
         INVITE (branch {pending_invite_branch}) was still in progress — {{Uas}} must \
         respond 500+Retry-After when the same sender sent both, 491 when they cross; \
         got {answered_status}/RetryAfter={retry_after} (RFC 3261 §14.2)",
    )
}

fn no_re_invite_while_invite_in_progress(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::OverlappingReInvite {
        branch,
        prior_branch,
        prior_accepted,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let phase = if *prior_accepted {
        "still Accepted — its 2xx not yet ACKed (RFC 6026)"
    } else {
        "still in progress"
    };
    format!(
        "Sent re-INVITE (callId {cid}, branch {branch}) while prior INVITE (branch \
             {prior_branch}) {phase} — {{Uac}} RFC 3261 §14.1 / RFC3261-MUST-083",
    )
}

fn failed_reinvite_tears_down_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::AbandonedReInvite {
        branch,
        provisional_status,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "re-INVITE (in-dialog INVITE, callId {cid}, branch {branch}) received a \
         {provisional_status} provisional but never a final response, and the dialog was \
         never BYE'd — the prior dialog state was silently torn down on a failed \
         re-INVITE; RFC 3261 §14.1 requires a failed re-INVITE to leave the dialog in \
         its prior state (and §17.1.1.2 a final response)"
    )
}
