//! The live surfacing of the well-formedness family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::BranchPrefix, branch_prefix),
    Live::emitter(RuleId::MaxForwards, max_forwards),
    Live::emitter(RuleId::ContentLength, content_length),
    Live::emitter(RuleId::ContentType, content_type),
    Live::emitter(RuleId::ContactPresence, contact_presence),
    Live::emitter(RuleId::NoContactOnBye, no_contact_on_bye),
    Live::emitter(RuleId::ToTagPresence, to_tag_presence),
    Live::emitter(RuleId::FromTagPresence, from_tag_presence),
    Live::emitter(RuleId::NoRecordRouteFromUa, no_record_route_from_ua).roles(&[UaRole::Proxy]),
    Live::emitter(RuleId::No100relRequireOnNonInvite, no_100rel_require_on_non_invite),
    // The reliable-provisional rows are the answering side's to write.
    Live::emitter(RuleId::Reliable1xxHeaders, reliable_1xx_headers).roles(&[UaRole::Uas]),
    Live::emitter(RuleId::CancelCseqMethod, cancel_cseq_method),
];

fn branch_prefix(f: &rfc_rules::Finding, _cid: &str) -> String {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) => {
            format!(
                        "{on} top Via branch \"{value}\" does not begin with the RFC 3261 magic \
                         cookie \"z9hG4bK\" (§8.1.1.7) — a downstream element cannot treat it as an \
                         RFC-3261 transaction id"
                    )
        }
        Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) => {
            format!(
                "{on} has no top Via branch parameter (RFC 3261 §8.1.1.7 requires a \
                             \"z9hG4bK\"-prefixed branch on every request)"
            )
        }
        _ => String::new(),
    }
}

fn max_forwards(f: &rfc_rules::Finding, _cid: &str) -> String {
    match &f.decision {
        Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) => {
            format!(
                        "{on} request is missing Max-Forwards — RFC 3261 §8.1.1.6 requires it on \
                             every request (a real downstream element cannot loop-protect this hop)"
                    )
        }
        Decision::Violated(rfc_rules::Evidence::HeaderValueRejected {
            on,
            value,
            expected,
            ..
        }) if expected == "at most 70" => format!(
            "{on} Max-Forwards is {value}, exceeds 70 — RFC 3261 §8.1.1.6 (70 is the \
                         recommended initial value; a higher count was minted, not decremented)"
        ),
        Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { on, value, .. }) => {
            format!(
                "{on} has an invalid Max-Forwards value \"{value}\" — RFC 3261 §8.1.1.6 \
                         requires an integer in 0..=255"
            )
        }
        _ => String::new(),
    }
}

fn content_length(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { value, expected, .. }) =
        &f.decision
    else {
        return String::new();
    };
    format!(
        "Content-Length mismatch: header says {value} but body is {expected} bytes — RFC \
         3261 §20.14 (a strict peer truncates or desyncs on this)"
    )
}

fn content_type(f: &rfc_rules::Finding, _cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    "message carries a body but no Content-Type — RFC 3261 §7.4.1 requires Content-Type \
     whenever a body is present (the peer cannot interpret the body)"
        .to_string()
}

fn contact_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request is missing Contact — RFC 3261 §8.1.1.8 requires it on \
         dialog-establishing methods (the dialog's remote target is undefined)"
    )
}

fn no_contact_on_bye(f: &rfc_rules::Finding, _cid: &str) -> String {
    if !f.violated() {
        return String::new();
    }
    "BYE carries Contact — RFC 3261 §15.1 (BYE terminates the dialog, so target-refresh \
     has no meaning)"
        .to_string()
}

fn to_tag_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} response is missing a To-tag — RFC 3261 §8.2.6.2 requires a UAS to add a \
         To-tag to every response above 100 (the peer cannot dialog-match it)"
    )
}

fn from_tag_presence(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RequiredHeaderAbsent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request is missing a From-tag — RFC 3261 §8.1.1.3 requires a UAC to add a \
         From-tag to every request (the peer cannot dialog-match it)"
    )
}

fn no_record_route_from_ua(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { value, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "B2BUA inserted Record-Route in a request — a B2BUA is a UA and MUST NOT use \
         Record-Route (RFC 3261 §16.6). Found: {value}"
    )
}

fn no_100rel_require_on_non_invite(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ForbiddenHeaderPresent { on, .. }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{on} request carries Require: 100rel — RFC 3262 §4 admits it on INVITE only (a \
         strict peer answers 420 Bad Extension)"
    )
}

fn reliable_1xx_headers(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::Reliable1xx { status, carried, .. }) = &f.decision
    else {
        return String::new();
    };
    if *status == 100 {
        return format!(
            "100 (Trying) response carries {} — RFC 3262 §3 forbids a reliable 100",
            carried.join(" and "),
        );
    }
    match carried.iter().find(|row| row.starts_with("RSeq:")) {
        None => format!(
            "Reliable {status} response carries Require: 100rel but no RSeq header — RFC \
             3262 §3 (the UAC has nothing to RAck)"
        ),
        Some(rseq) => {
            format!("Reliable {status} response {rseq} is outside [1, 2^31-1] — RFC 3262 §3")
        }
    }
}

fn cancel_cseq_method(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::HeaderValueRejected { value, .. }) = &f.decision
    else {
        return String::new();
    };
    format!("CANCEL request carries CSeq method={value} (expected CANCEL) — RFC 3261 §9.1")
}
