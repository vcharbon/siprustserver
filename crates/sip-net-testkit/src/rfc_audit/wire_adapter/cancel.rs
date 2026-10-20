//! The live surfacing of the CANCEL family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    // Live stamps are strictly monotonic, so a 2xx sent before the CANCEL arrived is a race the
    // rule clears.
    Live::emitter(RuleId::No200AfterCancel, no_200_after_cancel),
    Live::emitter(RuleId::CancelRouteEchoesInvite, cancel_route_echoes_invite)
        .skip(Skip::RelayLanes),
    // A `{Proxy}`-only lane forwards its upstream's CANCEL at the upstream's timing; a B2BUA
    // authors its own and is judged.
    Live::emitter(RuleId::CancelAfter1xx, cancel_after_1xx).skip(Skip::ProxyOnlyLanes),
    // No relay skip: a lane forwarding an upstream CANCEL never ACKed the final itself.
    Live::emitter(RuleId::NoCancelAfterFinal, no_cancel_after_final),
];

fn no_200_after_cancel(f: &rfc_rules::Finding, cid: &str) -> String {
    let (status, gap_us) = match &f.decision {
        Decision::Violated(rfc_rules::Evidence::Cancelled { status, gap_us, .. }) => {
            (*status, *gap_us)
        }
        _ => (0, 0),
    };
    format!(
        "Answered an INVITE it had already taken a CANCEL for with a {status} (callId \
         {cid}, CSeq {cseq}), {gap_ms} ms after that CANCEL — RFC 3261 §9.2 has the \
         cancelled UAS answer 487 Request Terminated",
        cseq = f.cseq,
        gap_ms = gap_us / 1_000,
    )
}

fn cancel_route_echoes_invite(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CancelRouteDiverged {
        cancel_routes,
        invite_routes,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent CANCEL Route values [{}] differ from INVITE Route values [{}] (callId \
         {cid}, branch {branch}) — RFC 3261 §9.1 / RFC3261-MUST-046",
        cancel_routes.join(", "),
        invite_routes.join(", "),
    )
}

fn cancel_after_1xx(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::EagerCancel { branch, .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "Sent CANCEL (callId {cid}, branch {branch}) before any received 1xx for the \
         INVITE — {{Uac}} RFC 3261 §9.1 / RFC3261-MUST-048",
    )
}

fn no_cancel_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::LateCancel {
        final_status, since_final_us, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent CANCEL (callId {cid}, CSeq {cseq}) {gap_ms} ms after ACKing the \
         {final_status} that completed its own INVITE transaction — {{Uac}} RFC 3261 \
         §9.1; §17.1.1.2 left no transaction to cancel and §9.2 answers the CANCEL 481",
        cseq = f.cseq,
        gap_ms = since_final_us / 1_000,
    )
}
