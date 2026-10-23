//! The live surfacing of the capability family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    // Advisory: both clauses are SHOULDs.
    Live::taker(RuleId::AllowSupportedOnInvite, allow_supported_on_invite)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::UnsupportedMethod405Allow, unsupported_method_405_allow),
    Live::emitter(RuleId::UnsupportedExtension420, unsupported_extension_420),
    Live::emitter(RuleId::Unsupported415Accepts, unsupported_415_accepts),
    Live::emitter(RuleId::UnsupportedExtension421, unsupported_extension_421),
    Live::emitter(RuleId::OptionsResponseEchoes, options_response_echoes),
    Live::emitter(RuleId::NoRequireOnCancelOrAck, no_require_on_cancel_or_ack),
];

fn allow_supported_on_invite(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CapabilitiesNotAdvertised {
        missing, status, ..
    }) = &f.decision
    else {
        return String::new();
    };
    let label = if *status == 0 { "re-INVITE".to_string() } else { format!("{status} OK INVITE") };
    missing
        .iter()
        .map(|h| match h.as_str() {
            "Allow" => format!(
                "{label} missing Allow: header — RFC 3261 §13.2.1 (SHOULD list accepted \
             methods)"
            ),
            _ => format!(
                "{label} missing Supported: header — RFC 3261 §20.37 (SHOULD list \
             extensions for Require negotiation)"
            ),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn unsupported_method_405_allow(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RejectionNotIssued {
        method,
        branch,
        answered_status,
        listed_rows,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Received unrecognised method {method} (Call-ID {cid}, branch {branch}) — \
         {{Uas}} must respond 405 with Allow header (RFC 3261 §8.2.1 / \
         RFC3261-MUST-030); got {answered_status} / Allow={listed_rows}",
    )
}

fn unsupported_extension_420(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RejectionNotIssued {
        method,
        branch,
        unsupported_tags,
        answered_status,
        listed_rows,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Received request {method} requires unsupported option tag(s) [{tags}] (callId \
         {cid}, branch {branch}) — {{Uas}} must respond 420 with Unsupported header; got \
         {answered_status} / Unsupported={listed_rows}",
        tags = unsupported_tags.join(", "),
    )
}

fn unsupported_415_accepts(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent 415 response (callId {cid}, branch {branch}) carries no Accept / \
         Accept-Encoding / Accept-Language header — {{Uas}} RFC 3261 §8.2.3 / \
         RFC3261-MUST-036",
    )
}

fn unsupported_extension_421(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent 421 response (callId {cid}, branch {branch}) lacks Require header listing \
         required extensions — {{Uas}} RFC 3261 §21.4.15 / RFC3261-MUST-182",
    )
}

fn options_response_echoes(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseHeadersMissing { branch, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent 2xx OPTIONS response (callId {cid}, branch {branch}) lacks \
         Allow/Supported/Accept headers — {{Uas}} RFC 3261 §11.2 / RFC3261-MUST-059",
    )
}

fn no_require_on_cancel_or_ack(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent {
        on, header, value, ..
    }) = &f.decision
    else {
        return String::new();
    };
    let what = if on.eq_ignore_ascii_case("ACK") {
        "ACK for a non-2xx response".to_string()
    } else {
        format!("{on} request")
    };
    format!(
        "{what} carries {header}: {value} — forbidden by RFC 3261 §8.2.2.3 (a \
         transaction-management request imposes no extension)"
    )
}
