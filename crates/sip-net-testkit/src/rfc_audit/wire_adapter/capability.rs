//! The live surfacing of the capability family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    // Advisory: both clauses are SHOULDs.
    Live::taker(RuleId::AllowSupportedOnInvite, allow_supported_on_invite)
        .advisory()
        .skip(Skip::RelayLanes),
    Live::emitter_unless(
        RuleId::UnsupportedMethod405Allow,
        rejection_405_lost,
        unsupported_method_405_allow,
    ),
    Live::emitter_unless(
        RuleId::UnsupportedExtension420,
        rejection_420_lost,
        unsupported_extension_420,
    ),
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

/// A 405 with `Allow` the loss model explains ([`rejection_explained`]).
fn rejection_405_lost(
    dropped: &Dropped,
    lane: &LaneKey,
    f: &rfc_rules::Finding,
    cid: &str,
    msgs: &[Msg],
) -> bool {
    rejection_explained(dropped, (lane, f, cid), msgs, (405, "Allow"))
}

/// A 420 with `Unsupported` the loss model explains ([`rejection_explained`]).
fn rejection_420_lost(
    dropped: &Dropped,
    lane: &LaneKey,
    f: &rfc_rules::Finding,
    cid: &str,
    msgs: &[Msg],
) -> bool {
    rejection_explained(dropped, (lane, f, cid), msgs, (420, "Unsupported"))
}

/// Whether the loss model explains the rejection the taker `lane` misses,
/// over its view `msgs`. With no final in view: the emitter's `status` final
/// carrying an `owed_header` row was dropped on its way to the lane, and its
/// §17.2.2 re-sent copy fell outside the view. With a 481 in view: the
/// request's copy was dropped on its way to the emitter, and the dialog had
/// ended in the lane's view before the 481 arrived, so the emitter took a
/// later copy on an ended dialog and owed it the 481 (§12.2.2). A wrong
/// final in view, a lost final of another status or without the owed
/// header, and a 481 with no lost request copy, are never excused.
fn rejection_explained(
    dropped: &Dropped,
    (lane, f, cid): (&LaneKey, &rfc_rules::Finding, &str),
    msgs: &[Msg],
    owed: (u16, &str),
) -> bool {
    let Decision::Violated(rfc_rules::Evidence::RejectionNotIssued {
        rejection_msg,
        method,
        answered_status,
        ..
    }) = &f.decision
    else {
        return false;
    };
    let ends = (lane.as_str(), f.emitter.as_str(), cid);
    match answered_status {
        0 => dropped.final_to(ends, (method, f.cseq), owed),
        481 => {
            dropped.request_from(ends, method, f.cseq)
                && answered_on_ended_dialog(msgs, &msgs[*rejection_msg])
        }
        _ => false,
    }
}

/// Whether, in the view `msgs`, a 2xx to a BYE on `request`'s dialog came
/// before the 481 its taker answered it with.
fn answered_on_ended_dialog(msgs: &[Msg], request: &Msg) -> bool {
    let answered_at = msgs.iter().find_map(|m| {
        let answers = m.status() == Some(481)
            && m.call_id == request.call_id
            && m.cseq == request.cseq
            && m.cseq_method == request.cseq_method
            && m.via_branch == request.via_branch;
        answers.then_some(m.at_us)
    });
    let Some(answered_at) = answered_at else { return false };
    msgs.iter().any(|m| {
        m.at_us < answered_at
            && m.cseq_method.eq_ignore_ascii_case("BYE")
            && m.status().is_some_and(|s| (200..300).contains(&s))
            && dialog(m) == dialog(request)
    })
}

/// The dialog `m` names: its Call-ID and its two tags in a fixed order, so
/// both directions of the dialog name it alike.
fn dialog(m: &Msg) -> (&str, [Option<&str>; 2]) {
    let mut tags = [m.from_tag.as_deref(), m.to_tag.as_deref()];
    tags.sort_unstable();
    (m.call_id.as_str(), tags)
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
