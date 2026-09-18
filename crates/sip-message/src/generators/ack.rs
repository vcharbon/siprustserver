//! ACK generation: for a 2xx (its own transaction, routed like an in-dialog
//! request — RFC 3261 §13.2.2.4) and for a non-2xx final (inside the INVITE
//! client transaction — §17.1.1.3).

use super::emit;
use super::in_dialog::{route_for_in_dialog, with_dialog_identity, with_routes};
use super::spec::{InviteClientTransactionHandle, StackDialog};
use crate::draft::RequestDraft;
use crate::header::{
    CSeq, ContentLength, HeaderName, HeaderValue, MaxForwards, MediaType, RouteEntry, Uri, Via,
};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// Inputs for [`generate_ack_for_2xx`]. Every header is a typed value; the
/// dialog contributes From / To / Call-ID / Route.
#[derive(Debug, Clone, Default)]
pub struct GenerateAckFor2xxOpts {
    /// This hop's own Via. Required.
    pub via: Option<Via>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Caller-stated header lines, carried verbatim (name spelling included).
    pub extra_headers: Vec<SipHeader>,
    /// Explicit CSeq override; required when `invite_txn` is `None`.
    pub cseq: Option<u32>,
    /// Remote-target override; defaults to `dialog.remote_target`.
    pub request_uri: Option<Uri>,
}

/// Build an ACK for a 2xx response. The CSeq number comes from the INVITE
/// (RFC 3261 §13.2.2.4), not from `dialog.local_cseq`; the dialog route set
/// is honoured like any in-dialog request. Panics when neither `invite_txn`
/// nor `opts.cseq` is provided.
pub fn generate_ack_for_2xx(
    invite_txn: Option<&InviteClientTransactionHandle>,
    dialog: &StackDialog,
    opts: &GenerateAckFor2xxOpts,
) -> SipRequest {
    let invite_cseq = opts
        .cseq
        .or_else(|| invite_txn.map(|t| t.original_invite.cseq().seq()))
        .expect("generate_ack_for_2xx: either invite_txn or opts.cseq must be provided");
    let remote_target =
        opts.request_uri.clone().unwrap_or_else(|| emit::uri(&dialog.remote_target));
    let (uri, routes) = route_for_in_dialog(remote_target, &dialog.route_set);
    let hop = opts.via.clone().expect("via required");

    let draft = RequestDraft::new(Method::Ack, uri).push(hop).push(MaxForwards::DEFAULT);
    let draft = with_dialog_identity(draft, dialog).push(CSeq::new(invite_cseq, Method::Ack));
    let draft = emit::extra_headers(with_routes(draft, &routes), &opts.extra_headers);

    emit::request(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}

/// Build an ACK for a non-2xx final response inside the INVITE client
/// transaction (RFC 3261 §17.1.1.3). Reuses the INVITE's topmost Via (same
/// branch); copies From / To / Call-ID from the response; CSeq method ACK with
/// the INVITE's sequence number; and reproduces the INVITE's Route headers
/// verbatim ("the Route header fields of the ACK MUST equal" the INVITE's) —
/// load-bearing when the INVITE carried a preloaded outbound-proxy Route
/// (RFC3261-MUST-145, flagged by the cross-message audit). `extra_headers` are
/// the caller's own lines, carried verbatim; this ACK carries no body — the
/// INVITE transaction absorbs it (§17.1.1.3) and no TU reads one.
pub fn generate_ack_for_non_2xx(
    original_invite: &SipRequest,
    final_response: &SipResponse,
    extra_headers: &[SipHeader],
) -> SipRequest {
    let via = original_invite
        .raw_text(HeaderName::Via)
        .next()
        .expect("generate_ack_for_non_2xx: INVITE missing Via");
    let echoed = |name: HeaderName| final_response.raw_text(name).next().unwrap_or(SipStr::EMPTY);

    let mut draft = RequestDraft::new(Method::Ack, original_invite.request_uri().clone())
        .push_raw(HeaderName::Via, via)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(HeaderName::From))
        .push_raw(HeaderName::To, echoed(HeaderName::To))
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId))
        .push(CSeq::new(original_invite.cseq().seq(), Method::Ack));
    for route in original_invite.raw_text(HeaderName::Route) {
        draft = draft.push_raw(HeaderName::Route, route);
    }

    emit::request(emit::extra_headers(draft, extra_headers).push(ContentLength::new(0)))
}

/// Build the bare ACK for a 2xx from the INVITE transaction alone — no dialog
/// record (RFC 3261 §13.2.2.4 for a UAC core that no longer holds one). Its
/// own transaction: the INVITE's top Via on the fresh `branch`. From and
/// Call-ID echo the INVITE, To echoes the 2xx (its tag), CSeq is the INVITE's
/// with method ACK. Routing follows §12.2.1.1 from what the two messages
/// state: an in-dialog INVITE (tagged To) keeps its Route set — a re-INVITE's
/// 2xx never changes it (§12.2.1.2) — and its Request-URI, unless the 2xx
/// refreshes the target with a Contact and the set is loose (or empty), in
/// which case the Contact is the Request-URI; a dialog-creating 2xx supplies
/// the route set (its Record-Route reversed, §12.1.2) and the target (its
/// Contact). No body: the round this ACK closes has no session left to
/// describe (RFC 3264 §4 answers ride only where a session survives).
pub fn generate_ack_for_2xx_from_invite(
    original_invite: &SipRequest,
    final_response: &SipResponse,
    branch: &str,
) -> SipRequest {
    let contact = final_response.contacts().as_slice().first().map(|c| c.uri().clone());
    let invite_routes: Vec<String> =
        original_invite.raw_text(HeaderName::Route).map(|r| r.to_string()).collect();
    let (uri, routes) = if original_invite.to().tag().is_some() {
        let loose = invite_routes
            .first()
            .map(|first| {
                RouteEntry::parse(&SipStr::owned(first)).is_ok_and(|e| e.uri().is_loose_route())
            })
            .unwrap_or(true);
        match contact {
            Some(target) if loose => (target, invite_routes),
            _ => (original_invite.request_uri().clone(), invite_routes),
        }
    } else {
        let route_set: Vec<String> = final_response
            .record_route_set()
            .map(|set| set.reversed().iter().map(HeaderValue::to_wire).collect())
            .unwrap_or_default();
        let target = contact.unwrap_or_else(|| original_invite.request_uri().clone());
        route_for_in_dialog(target, &route_set)
    };
    let echoed_from =
        |msg: &SipRequest, name: HeaderName| msg.raw_text(name).next().unwrap_or(SipStr::EMPTY);
    let hop = original_invite.top_via().clone().with_branch(branch.to_string());

    let mut draft = RequestDraft::new(Method::Ack, uri)
        .push(hop)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed_from(original_invite, HeaderName::From))
        .push_raw(
            HeaderName::To,
            final_response.raw_text(HeaderName::To).next().unwrap_or(SipStr::EMPTY),
        )
        .push_raw(HeaderName::CallId, echoed_from(original_invite, HeaderName::CallId))
        .push(CSeq::new(original_invite.cseq().seq(), Method::Ack));
    draft = with_routes(draft, &routes);

    emit::request(draft.push(ContentLength::new(0)))
}
