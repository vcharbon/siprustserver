//! The live surfacing of the correlation family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::taker(RuleId::ResponseEchoesRequestVia, response_echoes_request_via),
    Live::taker(RuleId::ResponseCorrelation, response_correlation),
    Live::taker(RuleId::MidDialogTags, mid_dialog_tags),
    Live::taker(RuleId::PeerUriStable, peer_uri_stable),
    Live::taker(RuleId::DialogCallIdStable, dialog_call_id_stable),
    Live::taker(RuleId::CancelRequestUri, cancel_request_uri),
    Live::taker(RuleId::CancelViaBranch, cancel_via_branch),
    Live::emitter(RuleId::TagConsistency, tag_flip_detail).advisory().admits(tag_flip_2xx),
    Live::emitter(RuleId::TagConsistency, tag_flip_detail).admits(tag_flip_non_2xx),
    Live::emitter(RuleId::NoToTagOnInitialRequest, no_to_tag_on_initial_request),
    // A declared proxy carries the originator's To through unchanged.
    Live::emitter(RuleId::InDialogToTag, in_dialog_to_tag).roles(&[UaRole::Uac, UaRole::Uas]),
    Live::taker_else_emitter(RuleId::InDialogFromTag, in_dialog_from_tag),
];

fn response_echoes_request_via(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseViaDiverged {
        response_branch,
        request_branch,
        response_vias,
        request_vias,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let mut parts = Vec::new();
    if response_branch != request_branch {
        parts.push(format!(
            "response top Via branch \"{response_branch}\" differs from the branch \
             this bind sent \"{request_branch}\" (the response cannot be matched to \
             its client transaction)"
        ));
    }
    if response_vias != request_vias {
        parts.push(format!(
            "response carries {response_vias} Via header(s) but the sent request had \
             {request_vias}"
        ));
    }
    format!(
        "{} — RFC 3261 §8.1.3 requires the response to echo the request's Via stack \
         unchanged",
        parts.join("; "),
    )
}

fn response_correlation(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseCseqPhantom {
        response_cseq,
        response_method,
        sent_cseqs,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "response CSeq {response_cseq} {response_method} does not echo any sent \
         {response_method} CSeq [{nums}] — RFC 3261 §8.1.3.3 (the peer is responding to a \
         phantom request)",
        nums = sent_cseqs.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
    )
}

fn mid_dialog_tags(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DialogTagForeign {
        tag_header,
        tag,
        local_tags,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let what = if tag_header == "To" { "in-dialog request" } else { "response" };
    format!(
        "{tag_header}-tag mismatch: {what} carries {tag_header}-tag \"{tag}\" but this \
         bind's local tags are [{expected}] — RFC 3261 §12.2.1.1",
        expected = local_tags.join(" | "),
    )
}

fn peer_uri_stable(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::PeerUriRewritten { sent_uri, dialog_uri, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "in-dialog From URI \"{sent_uri}\" differs from the dialog-established remote URI \
         \"{dialog_uri}\" — RFC 3261 §12.2.1.1"
    )
}

fn dialog_call_id_stable(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DialogCallIdChanged {
        call_id,
        dialog_call_id,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "received in-dialog message Call-ID \"{call_id}\" differs from the dialog's \
         confirmed Call-ID \"{dialog_call_id}\" — RFC 3261 §12.1 (Call-ID is immutable \
         within a dialog)"
    )
}

fn cancel_request_uri(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CancelUriDiverged {
        cancel_uri, invite_uri, ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "CANCEL Request-URI \"{cancel_uri}\" differs from the INVITE Request-URI \
         \"{invite_uri}\" — RFC 3261 §9.1 (the CANCEL cannot match the INVITE server \
         transaction)"
    )
}

fn cancel_via_branch(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::CancelBranchUnmatched {
        cancel_branch,
        invite_branches,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "CANCEL top Via branch \"{cancel_branch}\" matches no received INVITE's Via \
         branch (saw: {known}) — RFC 3261 §9.1 (the CANCEL is routed to a different \
         server transaction)",
        known = invite_branches.join(" | "),
    )
}

/// The status a violated `tag-consistency` finding flipped the tag on; `None`
/// for every other decision.
fn tag_flip_status(f: &rfc_rules::Finding) -> Option<u16> {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::UasTagFlipped { status, .. }) => Some(*status),
        _ => None,
    }
}

/// A flip on a 2xx: a forking B2BUA may answer 2xx off a later early dialog
/// (§12.1.2, §13.2.2.4), which per branch reads as a flip, so this arm advises.
fn tag_flip_2xx(f: &rfc_rules::Finding) -> bool {
    tag_flip_status(f).is_some_and(|status| (200..300).contains(&status))
}

/// A flip on a non-2xx final: it establishes no dialog (§12.1.1), so the
/// caller's early dialog is never terminated, and this arm gates.
fn tag_flip_non_2xx(f: &rfc_rules::Finding) -> bool {
    tag_flip_status(f).is_some_and(|status| !(200..300).contains(&status))
}

fn no_to_tag_on_initial_request(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { on, value, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request outside any dialog carries To-tag={value} — RFC 3261 §8.1.1.2 (it \
         names a dialog that does not exist)"
    )
}

fn in_dialog_to_tag(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request inside a confirmed dialog carries no To-tag — RFC 3261 §12.2.1.1 \
         requires the dialog's remote tag (the peer cannot dialog-match it)"
    )
}

fn in_dialog_from_tag(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::DialogRemoteTagForeign {
        method,
        from_tag,
        dialog_remote_tags,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{method} (callId {cid}, CSeq {cseq}) taken inside a confirmed dialog carries \
         From-tag \"{from_tag}\", but the dialogs confirmed under its To-tag have \
         remote tags [{remotes}] — RFC 3261 §12.2.1.1 / §12.1.2 (the request names no \
         dialog its taker holds)",
        cseq = f.cseq,
        remotes = dialog_remote_tags.join(" | "),
    )
}
