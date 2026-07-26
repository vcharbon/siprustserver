//! In-dialog request generation (RFC 3261 §12.2.1.1): BYE, re-INVITE, PRACK,
//! NOTIFY, INFO, UPDATE, MESSAGE, REFER — plus the loose/strict Request-URI +
//! Route-set computation shared with the ACK generator.

use super::emit;
use super::methods::{InDialogMethod, B2BUA_ALLOW, B2BUA_SUPPORTED};
use super::spec::{ContactSpec, StackDialog, ViaSpec};
use crate::draft::RequestDraft;
use crate::header::{
    self, CSeq, CallId, Event, HeaderName, MaxForwards, MediaType, RAck, SubscriptionState, Uri,
    Via,
};
use crate::message_helpers::route::{first_route_is_loose, strip_route_uri_to_request_uri};
use crate::method::Method;
use crate::sip_str::SipStr;
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

/// Put a route set on the request, one Route line per entry, in order.
pub(super) fn with_routes(mut draft: RequestDraft, routes: &[String]) -> RequestDraft {
    for route in routes {
        draft = draft.push_raw(HeaderName::Route, SipStr::owned(route));
    }
    draft
}

/// The dialog's identity headers (RFC 3261 §12.2.1.1): local identity plus
/// local tag in From, remote identity plus remote tag in To, then the Call-ID.
/// A dialog hydrated mid-confirm by a reactive failover takeover can carry an
/// EMPTY remote tag; the To then goes out tag-less rather than malformed, and
/// the degenerate dialog is handled elsewhere.
pub(super) fn with_dialog_identity(draft: RequestDraft, dialog: &StackDialog) -> RequestDraft {
    draft
        .push_raw(HeaderName::From, emit::name_addr_text(&dialog.local_uri, Some(&dialog.local_tag)))
        .push_raw(
            HeaderName::To,
            emit::name_addr_text(&dialog.remote_uri, Some(&dialog.remote_tag)),
        )
        .push(CallId::new(SipStr::owned(&dialog.call_id)))
}

/// The typed twin of the stringly fields of [`GenerateInDialogRequestOpts`]:
/// each value supersedes the text that names the same header.
#[derive(Debug, Clone, Default)]
pub struct InDialogValues {
    /// Supersedes `request_uri`.
    pub uri: Option<Uri>,
    /// Supersedes `via`.
    pub hop: Option<Via>,
    /// Supersedes `contact`.
    pub contact: Option<header::Contact>,
    /// Supersedes `rack`.
    pub rack: Option<RAck>,
    /// Supersedes `event`.
    pub event: Option<Event>,
    /// Supersedes `subscription_state`.
    pub subscription_state: Option<SubscriptionState>,
    /// Supersedes `content_type`.
    pub content_type: Option<MediaType>,
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
    /// Typed values, each superseding its stringly counterpart above.
    pub values: InDialogValues,
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
    let values = &opts.values;
    let next_cseq = opts.cseq.unwrap_or(dialog.local_cseq + 1);
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    let (request_uri, routes) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let uri = values.uri.clone().unwrap_or_else(|| emit::uri(&request_uri));
    let hop =
        values.hop.clone().unwrap_or_else(|| opts.via.as_ref().expect("ViaSpec required").value());
    let verb = Method::from(method);

    let mut draft = RequestDraft::new(verb.clone(), uri)
        .push(hop)
        .push(MaxForwards::new(emit::DEFAULT_MAX_FORWARDS));
    draft = with_dialog_identity(draft, dialog).push(CSeq::new(next_cseq, verb));

    // Contact for every in-dialog method EXCEPT BYE (RFC 3261 §15.1).
    if method != InDialogMethod::Bye {
        let contact = values
            .contact
            .clone()
            .unwrap_or_else(|| opts.contact.as_ref().expect("ContactSpec required").value());
        draft = draft.push(contact);
    }

    draft = with_routes(draft, &routes);

    if method == InDialogMethod::Prack {
        draft = match (&values.rack, &opts.rack) {
            (Some(rack), _) => draft.push(rack.clone()),
            (None, Some(text)) => draft.push_raw(HeaderName::RAck, SipStr::owned(text)),
            (None, None) => draft,
        };
    }
    if method == InDialogMethod::Notify {
        draft = match (&values.event, &opts.event) {
            (Some(event), _) => draft.push(event.clone()),
            (None, Some(text)) => draft.push_raw(HeaderName::Event, SipStr::owned(text)),
            (None, None) => draft,
        };
        draft = match (&values.subscription_state, &opts.subscription_state) {
            (Some(state), _) => draft.push(state.clone()),
            (None, Some(text)) => {
                draft.push_raw(HeaderName::SubscriptionState, SipStr::owned(text))
            }
            (None, None) => draft,
        };
    }
    if method == InDialogMethod::Invite {
        // Advertise capabilities — but never duplicate a header the caller
        // already carries through `extra_headers` (duplicated values merge per
        // RFC 3261 §7.3.1).
        if !emit::carries(&opts.extra_headers, &HeaderName::Allow) {
            draft = draft.push_raw(HeaderName::Allow, SipStr::from_static(B2BUA_ALLOW));
        }
        if !emit::carries(&opts.extra_headers, &HeaderName::Supported) {
            draft = draft.push_raw(HeaderName::Supported, SipStr::from_static(B2BUA_SUPPORTED));
        }
    }

    draft = emit::extra_headers(draft, &opts.extra_headers);
    let content_type = emit::media_type(&values.content_type, &opts.content_type);
    let request = emit::request(emit::framed(draft, opts.body.clone(), content_type));
    let next_dialog = StackDialog { local_cseq: next_cseq, ..dialog.clone() };
    InDialogResult { request, dialog: next_dialog }
}
