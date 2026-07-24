//! In-dialog request generation (RFC 3261 §12.2.1.1): BYE, re-INVITE, PRACK,
//! NOTIFY, INFO, UPDATE, MESSAGE, REFER — plus the loose/strict Request-URI +
//! Route-set computation shared with the ACK generator.

use super::emit::{append_body_headers, h, make_request, wrap_uri};
use super::methods::{InDialogMethod, B2BUA_ALLOW, B2BUA_SUPPORTED};
use super::spec::{ContactSpec, StackDialog, ViaSpec};
use crate::message_helpers::route::{first_route_is_loose, strip_route_uri_to_request_uri};
use crate::types::{SipHeader, SipRequest};

/// Compute the Request-URI and ordered Route header values for an in-dialog
/// request, given the dialog's remote target and route set (RFC 3261
/// §12.2.1.1 / §16.12):
///   - empty route set → `(remote_target, [])`;
///   - loose (first route has `;lr`) → `(remote_target, route_set)` as-is;
///   - strict → `(first route URI, rest of route_set ++ <remote_target>)`.
pub(super) fn route_for_in_dialog(
    remote_target: &str,
    route_set: &[String],
) -> (String, Vec<String>) {
    if route_set.is_empty() {
        return (remote_target.to_string(), Vec::new());
    }
    if first_route_is_loose(&route_set[0]) {
        (remote_target.to_string(), route_set.to_vec())
    } else {
        let request_uri = strip_route_uri_to_request_uri(&route_set[0]);
        let mut routes: Vec<String> = route_set[1..].to_vec();
        routes.push(format!("<{remote_target}>"));
        (request_uri, routes)
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenerateInDialogRequestOpts {
    pub via: Option<ViaSpec>,
    pub contact: Option<ContactSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Required when method == PRACK (RFC 3262).
    pub rack: Option<String>,
    /// Required when method == NOTIFY (RFC 6665 §7.2).
    pub event: Option<String>,
    /// Required when method == NOTIFY (RFC 6665 §4.1.3).
    pub subscription_state: Option<String>,
    /// Explicit CSeq override; defaults to `dialog.local_cseq + 1`.
    pub cseq: Option<u32>,
    /// Request-URI override; defaults to `dialog.remote_target`.
    pub request_uri: Option<String>,
}

/// Result of [`generate_in_dialog_request`]: the request plus the dialog with
/// `local_cseq` bumped to the used CSeq (callers persist the new dialog).
pub struct InDialogResult {
    pub request: SipRequest,
    pub dialog: StackDialog,
}

/// Build an in-dialog request (RFC 3261 §12.2.1.1).
pub fn generate_in_dialog_request(
    method: InDialogMethod,
    dialog: &StackDialog,
    opts: &GenerateInDialogRequestOpts,
) -> InDialogResult {
    let body = opts.body.clone();
    let next_cseq = opts.cseq.unwrap_or(dialog.local_cseq + 1);
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    let (request_uri, route_values) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let via = opts.via.as_ref().expect("ViaSpec required");

    // RFC 3261 §12.2.1.1: the To header carries the remote tag. A dialog
    // hydrated mid-confirm by a failover takeover can have an EMPTY
    // `remote_tag`; `;tag=` with an empty value is malformed (hydrate_request
    // rejects it), so the tag is skipped when absent — the request stays
    // well-formed and the degenerate dialog is handled elsewhere.
    let to_value = if dialog.remote_tag.is_empty() {
        wrap_uri(&dialog.remote_uri)
    } else {
        format!("{};tag={}", wrap_uri(&dialog.remote_uri), dialog.remote_tag)
    };
    let mut headers: Vec<SipHeader> = vec![
        h("Via", via.header_value()),
        h("Max-Forwards", "70"),
        h("From", format!("{};tag={}", wrap_uri(&dialog.local_uri), dialog.local_tag)),
        h("To", to_value),
        h("Call-ID", dialog.call_id.clone()),
        h("CSeq", format!("{} {}", next_cseq, method.as_str())),
    ];

    // Contact for every in-dialog method EXCEPT BYE (RFC 3261 §15.1).
    if method != InDialogMethod::Bye {
        let contact = opts.contact.as_ref().expect("ContactSpec required");
        headers.push(h("Contact", contact.header_value()));
    }

    for route in &route_values {
        headers.push(h("Route", route.clone()));
    }

    if method == InDialogMethod::Prack {
        if let Some(rack) = &opts.rack {
            headers.push(h("RAck", rack.clone()));
        }
    }
    if method == InDialogMethod::Notify {
        if let Some(event) = &opts.event {
            headers.push(h("Event", event.clone()));
        }
        if let Some(ss) = &opts.subscription_state {
            headers.push(h("Subscription-State", ss.clone()));
        }
    }
    if method == InDialogMethod::Invite {
        // Advertise capabilities — but never duplicate a header the caller
        // already carries through `extra_headers` (duplicated values merge per
        // RFC 3261 §7.3.1). The probe is compact-form-aware (§7.3.3): a
        // carried `k:` (compact Supported) suppresses the stack default.
        let carried = |name: &str| {
            opts.extra_headers.iter().any(|hdr| crate::message_helpers::name_matches(name, &hdr.name))
        };
        if !carried("Allow") {
            headers.push(h("Allow", B2BUA_ALLOW));
        }
        if !carried("Supported") {
            headers.push(h("Supported", B2BUA_SUPPORTED));
        }
    }

    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    let request = make_request(method.as_str(), &request_uri, headers, body);
    let next_dialog = StackDialog { local_cseq: next_cseq, ..dialog.clone() };
    InDialogResult { request, dialog: next_dialog }
}
