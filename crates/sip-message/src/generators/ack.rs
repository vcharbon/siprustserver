//! ACK generation: for a 2xx (its own transaction, routed like an in-dialog
//! request — RFC 3261 §13.2.2.4) and for a non-2xx final (inside the INVITE
//! client transaction — §17.1.1.3).

use super::emit;
use super::in_dialog::{route_for_in_dialog, with_dialog_identity, with_routes};
use super::spec::{InviteClientTransactionHandle, StackDialog, ViaSpec};
use crate::draft::RequestDraft;
use crate::header::{CSeq, ContentLength, HeaderName, MaxForwards, MediaType, Uri, Via};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// The typed twin of the stringly fields of [`GenerateAckFor2xxOpts`]: each
/// value supersedes the text that names the same header.
#[derive(Debug, Clone, Default)]
pub struct AckFor2xxValues {
    /// Supersedes `request_uri`.
    pub uri: Option<Uri>,
    /// Supersedes `via`.
    pub hop: Option<Via>,
    /// Supersedes `content_type`.
    pub content_type: Option<MediaType>,
}

#[derive(Debug, Clone, Default)]
pub struct GenerateAckFor2xxOpts {
    pub via: Option<ViaSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Explicit CSeq override; required when `invite_txn` is `None`.
    pub cseq: Option<u32>,
    /// Request-URI override; defaults to `dialog.remote_target`.
    pub request_uri: Option<String>,
    /// Typed values, each superseding its stringly counterpart above.
    pub values: AckFor2xxValues,
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
    let values = &opts.values;
    let invite_cseq = opts
        .cseq
        .or_else(|| invite_txn.map(|t| t.original_invite.cseq().seq()))
        .expect("generate_ack_for_2xx: either invite_txn or opts.cseq must be provided");
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    let (request_uri, routes) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let uri = values.uri.clone().unwrap_or_else(|| emit::uri(&request_uri));
    let hop =
        values.hop.clone().unwrap_or_else(|| opts.via.as_ref().expect("ViaSpec required").value());

    let draft = RequestDraft::new(Method::Ack, uri)
        .push(hop)
        .push(MaxForwards::new(emit::DEFAULT_MAX_FORWARDS));
    let draft = with_dialog_identity(draft, dialog).push(CSeq::new(invite_cseq, Method::Ack));
    let draft = emit::extra_headers(with_routes(draft, &routes), &opts.extra_headers);

    let content_type = emit::media_type(&values.content_type, &opts.content_type);
    emit::request(emit::framed(draft, opts.body.clone(), content_type))
}

/// Build an ACK for a non-2xx final response inside the INVITE client
/// transaction (RFC 3261 §17.1.1.3). Reuses the INVITE's topmost Via (same
/// branch); copies From / To / Call-ID from the response; CSeq method ACK with
/// the INVITE's sequence number; and reproduces the INVITE's Route headers
/// verbatim ("the Route header fields of the ACK MUST equal" the INVITE's) —
/// load-bearing when the INVITE carried a preloaded outbound-proxy Route
/// (RFC3261-MUST-145, flagged by the cross-message audit).
pub fn generate_ack_for_non_2xx(
    original_invite: &SipRequest,
    final_response: &SipResponse,
) -> SipRequest {
    let via = original_invite
        .raw_text(HeaderName::Via)
        .next()
        .expect("generate_ack_for_non_2xx: INVITE missing Via");
    let echoed = |name: HeaderName| final_response.raw_text(name).next().unwrap_or(SipStr::EMPTY);

    let mut draft = RequestDraft::new(Method::Ack, original_invite.request_uri().clone())
        .push_raw(HeaderName::Via, via)
        .push(MaxForwards::new(emit::DEFAULT_MAX_FORWARDS))
        .push_raw(HeaderName::From, echoed(HeaderName::From))
        .push_raw(HeaderName::To, echoed(HeaderName::To))
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId))
        .push(CSeq::new(original_invite.cseq().seq(), Method::Ack));
    for route in original_invite.raw_text(HeaderName::Route) {
        draft = draft.push_raw(HeaderName::Route, route);
    }

    emit::request(draft.push(ContentLength::new(0)))
}
