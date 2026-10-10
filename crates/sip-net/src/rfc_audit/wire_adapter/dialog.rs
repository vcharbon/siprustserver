//! The live surfacing of the dialog family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::MidDialogUri, mid_dialog_uri).skip(Skip::RelayLanes),
    Live::emitter(RuleId::MidDialogRoute, mid_dialog_route).skip(Skip::RelayLanes),
    Live::emitter(RuleId::MidDialogWireDestination, mid_dialog_wire_destination)
        .skip(Skip::RelayLanes),
    Live::taker(RuleId::RecordRoutePlacement, record_route_placement).skip(Skip::RelayLanes),
    // No relay skip: a relay face knows both its peers, so the rule's peer-tag key never charges
    // it.
    Live::emitter(RuleId::UnknownDialog481, unknown_dialog_481),
    // No relay skip: the peer-tag dialog key spares a face that saw the dialog created.
    Live::emitter(RuleId::NoByeOutsideOrEarlyDialog, no_bye_outside_or_early_dialog),
];

fn mid_dialog_uri(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::MidDialogUriChanged {
        method,
        sent_from_uri,
        dialog_local_uri,
        sent_to_uri,
        dialog_remote_uri,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let mut parts = Vec::new();
    if sent_from_uri != dialog_local_uri {
        parts.push(format!(
            "in-dialog {method} From URI \"{sent_from_uri}\" differs from dialog local \
             URI \"{dialog_local_uri}\""
        ));
    }
    if sent_to_uri != dialog_remote_uri {
        parts.push(format!(
            "in-dialog {method} To URI \"{sent_to_uri}\" differs from dialog remote URI \
             \"{dialog_remote_uri}\""
        ));
    }
    format!("{} — RFC 3261 §12.2.1.1", parts.join("; "))
}

fn mid_dialog_route(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::MidDialogRouteDiverged {
        method,
        dialog_route_set,
        sent_routes,
        loose_first_route,
        request_uri,
        first_bad_hop,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    if *loose_first_route {
        let first = dialog_route_set.first().map(String::as_str).unwrap_or_default();
        if sent_routes.is_empty() {
            return format!(
                "in-dialog {method} omits Route header although the dialog route set is \
                 non-empty (loose, first entry \"{first}\") — RFC 3261 §12.2.1.1"
            );
        }
        if sent_routes.len() != dialog_route_set.len() {
            return format!(
                "in-dialog {method} Route header count {} differs from dialog route set \
                 length {} — RFC 3261 §12.2.1.1",
                sent_routes.len(),
                dialog_route_set.len(),
            );
        }
        let i = first_bad_hop.unwrap_or(0);
        return format!(
            "in-dialog {method} Route[{i}] \"{}\" routes to a different host:port than \
             dialog route set entry \"{}\" — RFC 3261 §12.2.1.1",
            sent_routes.get(i).map(String::as_str).unwrap_or_default(),
            dialog_route_set.get(i).map(String::as_str).unwrap_or_default(),
        );
    }
    let expected = dialog_route_set.first().map(String::as_str).unwrap_or_default();
    let mut parts = Vec::new();
    if request_uri != expected {
        parts.push(format!(
            "in-dialog {method} Request-URI \"{request_uri}\" should be first strict \
             route URI \"{expected}\""
        ));
    }
    let tail = &dialog_route_set[1.min(dialog_route_set.len())..];
    if !(sent_routes.as_slice() == tail
        || (sent_routes.len() == tail.len() + 1 && sent_routes[..tail.len()] == *tail))
    {
        parts.push(format!(
            "in-dialog {method} strict-route Route tail does not match dialog route set \
             (expected {tail:?}, got {sent_routes:?})"
        ));
    }
    format!("{} — RFC 3261 §16.12", parts.join("; "))
}

fn mid_dialog_wire_destination(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::MidDialogWireTargetDiverged {
        method,
        sent_to,
        target_uri,
        target_host,
        target_port,
        from_route,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let lead =
        if *from_route { "first route URI resolves to " } else { "Request-URI resolves to " };
    format!(
        "in-dialog {method} wire-sent to {sent_to} but {lead}{target_host}:{target_port} \
         (\"{target_uri}\") — RFC 3261 §8.1.2 + RFC 3263 §4"
    )
}

fn record_route_placement(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RecordRouteMisplaced {
        status,
        record_route,
        request_method,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    if *status == 100 {
        return format!(
            "100 Trying carries Record-Route header(s) — 100 is not dialog-creating, \
             Record-Route is vestigial here per RFC 3261 §12.1.1. Found: {record_route}"
        );
    }
    format!(
        "{status} response to in-dialog {request_method} carries Record-Route — route \
         set is fixed at dialog establishment per RFC 3261 §12.2.2. Found: {record_route}"
    )
}

fn unknown_dialog_481(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::UnknownDialogRequest {
        method,
        from_tag,
        to_tag,
        answered_status,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let answered = match answered_status {
        0 => "nothing at all".to_string(),
        st => st.to_string(),
    };
    format!(
        "Received in-dialog request {method} for unknown dialog \
         {cid}/{from_tag}/{to_tag} — {{Uas}} must respond 481 (RFC 3261 §12.2.2 / \
         RFC3261-MUST-071); answered {answered}"
    )
}

fn no_bye_outside_or_early_dialog(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ByeOffDialog { early_dialog, .. }) = &f.decision
    else {
        return String::new();
    };
    if *early_dialog {
        format!(
            "Callee sent BYE on early dialog (callId {cid}) — should use CANCEL or \
             4xx/5xx/6xx (RFC 3261 §15)"
        )
    } else {
        format!(
            "Sent BYE (callId {cid}) outside any dialog — RFC 3261 §15 / \
             RFC3261-MUST-089"
        )
    }
}
