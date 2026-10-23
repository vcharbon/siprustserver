//! The live surfacing of the proxy family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::taker(RuleId::Proxy100TryingNotForwarded, proxy_100_trying_not_forwarded)
        .skip(Skip::RelayLanes),
    // §16.3 is proxy behaviour: a UAS rejecting a call it took is no resolution failure.
    Live::emitter(RuleId::NoTarget404, no_target_404).roles(&[UaRole::Proxy]).advisory(),
    // §16.4 is proxy behaviour.
    Live::emitter(RuleId::StrictRouteRewriteHandled, strict_route_rewrite_handled)
        .roles(&[UaRole::Proxy]),
    // §16.7 binds proxies; a UAS answering an INVITE is under §8.2.6.
    Live::emitter(RuleId::Proxy100WithinGrace, proxy_100_within_grace)
        .roles(&[UaRole::Proxy])
        .advisory(),
    // §16.6 step 6 is a forwarding element's step.
    Live::emitter(RuleId::StrictRouteShuffleOnSend, strict_route_shuffle_on_send)
        .roles(&[UaRole::Proxy]),
];

fn proxy_100_trying_not_forwarded(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ExtraTryingForwarded {
        trying_taken,
        invites_sent,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "received {trying_taken} 100 Trying for INVITE CSeq={cseq} Call-ID={cid} against \
         {invites_sent} INVITE(s) sent — {sender} sent a 100 beyond one per INVITE: a \
         stateful proxy forwarding a downstream 100 it should absorb (RFC 3261 §16.7 \
         step 5), or a UAS answering one received INVITE twice (§17.2.1)",
        cseq = f.cseq,
        sender = f.emitter,
    )
}

fn no_target_404(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::NoTargetFinal { method, branch, status, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "{{Proxy}} received request {method} (callId {cid}, branch {branch}) and emitted \
         final response {status} without forwarding — expected 404 (RFC 3261 §16.3 / \
         RFC3261-MUST-105)"
    )
}

fn strict_route_rewrite_handled(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::StrictRouteNotRewritten {
        branch,
        first_route,
        forwarded_request_uri,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let forwarded = if forwarded_request_uri.is_empty() { "<none>" } else { forwarded_request_uri };
    format!(
        "{{Proxy}} received strict-route request (callId {cid}, branch {branch}; first \
         Route={first_route}) but outgoing Request-URI={forwarded} — RFC 3261 §16.4 / \
         RFC3261-MUST-100"
    )
}

fn proxy_100_within_grace(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::TryingNotSentInGrace {
        branch,
        first_final_after_us,
        grace_us,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let observed = match first_final_after_us {
        Some(us) => format!("first final after Δ={}ms", us / 1_000),
        None => "no response sent".to_string(),
    };
    format!(
        "{{Proxy}} did not emit 100 Trying within {}ms of INVITE receipt (callId {cid}, \
         branch {branch}; {observed}) — RFC 3261 §16.7 / RFC3261-MUST-095",
        grace_us / 1_000,
    )
}

fn strict_route_shuffle_on_send(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent {on} request still carries strict-route topmost Route entry \"{value}\" — \
         RFC 3261 §16.6 step 6 swap may not have run",
    )
}
