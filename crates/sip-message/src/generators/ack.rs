//! ACK generation: for a 2xx (its own transaction, routed like an in-dialog
//! request — RFC 3261 §13.2.2.4) and for a non-2xx final (inside the INVITE
//! client transaction — §17.1.1.3).

use super::emit::{append_body_headers, h, make_request, wrap_uri};
use super::in_dialog::route_for_in_dialog;
use super::spec::{InviteClientTransactionHandle, StackDialog, ViaSpec};
use crate::message_helpers::{get_header, get_headers};
use crate::types::{SipHeader, SipRequest, SipResponse};

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
    let body = opts.body.clone();
    let invite_cseq = opts
        .cseq
        .or_else(|| invite_txn.map(|t| t.original_invite.cseq.seq))
        .expect("generate_ack_for_2xx: either invite_txn or opts.cseq must be provided");
    let remote_target = opts.request_uri.clone().unwrap_or_else(|| dialog.remote_target.clone());
    let (request_uri, route_values) = route_for_in_dialog(&remote_target, &dialog.route_set);
    let via = opts.via.as_ref().expect("ViaSpec required");

    // RFC 3261 §12.2.1.1: the To header carries the remote tag. A dialog
    // hydrated mid-confirm by a reactive failover takeover (relayed 2xx not
    // yet seen) can have an EMPTY `remote_tag`; `;tag=` with an empty value is
    // malformed (hydrate_request rejects it), so the tag is skipped when
    // absent — same contract as `generate_in_dialog_request`.
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
        h("CSeq", format!("{invite_cseq} ACK")),
    ];
    for route in &route_values {
        headers.push(h("Route", route.clone()));
    }
    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_request("ACK", &request_uri, headers, body)
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
    let via = get_header(&original_invite.headers, "via")
        .expect("generate_ack_for_non_2xx: INVITE missing Via");
    let from = get_header(&final_response.headers, "from").unwrap_or("");
    let to = get_header(&final_response.headers, "to").unwrap_or("");
    let call_id = get_header(&final_response.headers, "call-id").unwrap_or("");
    let cseq_num = original_invite.cseq.seq;

    let mut headers: Vec<SipHeader> = vec![
        h("Via", via),
        h("Max-Forwards", "70"),
        h("From", from),
        h("To", to),
        h("Call-ID", call_id),
        h("CSeq", format!("{cseq_num} ACK")),
    ];
    for route in get_headers(&original_invite.headers, "route") {
        headers.push(h("Route", route));
    }
    headers.push(h("Content-Length", "0"));

    make_request("ACK", &original_invite.uri, headers, Vec::new())
}
