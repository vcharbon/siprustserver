//! In-dialog request generation (RFC 3261 §12.2.1.1): BYE, re-INVITE, PRACK,
//! NOTIFY, INFO, UPDATE, MESSAGE, REFER — plus the loose/strict Request-URI +
//! Route-set computation shared with the ACK generator.

use super::emit;
use super::methods::{InDialogMethod, B2BUA_ALLOW, B2BUA_SUPPORTED};
use super::spec::StackDialog;
use crate::draft::RequestDraft;
use crate::header::{
    self, CSeq, CallId, Event, HeaderName, HeaderValue, MaxForwards, MediaType, RAck, RouteEntry,
    SubscriptionState, Uri, Via,
};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest};

/// Compute the Request-URI and ordered Route header values for an in-dialog
/// request, given the dialog's remote target and route set (RFC 3261
/// §12.2.1.1 / §16.12):
///   - empty route set → `(remote_target, [])`;
///   - loose (the first route's URI carries `;lr`) → `(remote_target,
///     route_set)` as-is;
///   - strict → `(first route URI, rest of route_set ++ <remote_target>)`.
///
/// A first route no reader accepts is carried through untouched, so an
/// unreadable entry never redirects the request at itself.
pub(super) fn route_for_in_dialog(remote_target: Uri, route_set: &[String]) -> (Uri, Vec<String>) {
    let Some(first) = route_set.first() else {
        return (remote_target, Vec::new());
    };
    match RouteEntry::parse(&SipStr::owned(first)) {
        Ok(entry) if !entry.uri().is_loose_route() => {
            let request_uri = entry.uri().clone();
            let mut routes: Vec<String> = route_set[1..].to_vec();
            routes.push(format!("<{}>", remote_target.text()));
            (request_uri, routes)
        }
        _ => (remote_target, route_set.to_vec()),
    }
}

/// Put a route set on the request, one Route line per entry, in order. The
/// dialog stores its route set as text (it learned it from the wire), so each
/// line rides verbatim.
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

/// Inputs for [`generate_in_dialog_request`]. Every header is a typed value;
/// the dialog contributes From / To / Call-ID / Route.
#[derive(Debug, Clone, Default)]
pub struct GenerateInDialogRequestOpts {
    /// This hop's own Via. Required.
    pub via: Option<Via>,
    /// Contact — required for every in-dialog method except BYE (§15.1).
    pub contact: Option<header::Contact>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Caller-stated header lines, carried verbatim (name spelling included).
    pub extra_headers: Vec<SipHeader>,
    /// Required when method == PRACK (RFC 3262).
    pub rack: Option<RAck>,
    /// Required when method == NOTIFY (RFC 6665 §7.2).
    pub event: Option<Event>,
    /// Required when method == NOTIFY (RFC 6665 §4.1.3).
    pub subscription_state: Option<SubscriptionState>,
    /// Explicit CSeq override; defaults to `dialog.local_cseq + 1`.
    pub cseq: Option<u32>,
    /// Remote-target override; defaults to `dialog.remote_target`. The route
    /// set still decides the Request-URI (§12.2.1.1).
    pub request_uri: Option<Uri>,
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
    let next_cseq = opts.cseq.unwrap_or(dialog.local_cseq + 1);
    let remote_target =
        opts.request_uri.clone().unwrap_or_else(|| emit::uri(&dialog.remote_target));
    let (uri, routes) = route_for_in_dialog(remote_target, &dialog.route_set);
    let hop = opts.via.clone().expect("via required");
    let verb = Method::from(method);

    let mut draft = RequestDraft::new(verb.clone(), uri).push(hop).push(MaxForwards::DEFAULT);
    draft = with_dialog_identity(draft, dialog).push(CSeq::new(next_cseq, verb));

    // Contact for every in-dialog method EXCEPT BYE (RFC 3261 §15.1).
    if method != InDialogMethod::Bye {
        draft = draft.push(opts.contact.clone().expect("contact required"));
    }

    draft = with_routes(draft, &routes);

    if method == InDialogMethod::Prack {
        if let Some(rack) = &opts.rack {
            draft = draft.push(rack.clone());
        }
    }
    if method == InDialogMethod::Notify {
        if let Some(event) = &opts.event {
            draft = draft.push(event.clone());
        }
        if let Some(state) = &opts.subscription_state {
            draft = draft.push(state.clone());
        }
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
    let request =
        emit::request(emit::framed(draft, opts.body.clone(), opts.content_type.clone()));
    let next_dialog = StackDialog { local_cseq: next_cseq, ..dialog.clone() };
    InDialogResult { request, dialog: next_dialog }
}
