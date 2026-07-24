//! CANCEL generation for an outstanding INVITE (RFC 3261 §9.1).

use super::emit::{h, make_request};
use super::spec::InviteClientTransactionHandle;
use crate::message_helpers::{get_header, get_headers};
use crate::types::{SipHeader, SipRequest};

/// Build a CANCEL for the outstanding INVITE (RFC 3261 §9.1): the CANCEL "MUST
/// have a single Via header field value, and that value MUST equal the top Via
/// header field value of the request being cancelled" — copied verbatim (same
/// branch, the transaction-correlation key); Request-URI / Call-ID / From / To
/// echo the INVITE; CSeq number reused with method CANCEL. §9.1 also requires
/// the CANCEL to be routed the SAME way as the INVITE — "the Route header fields
/// of the CANCEL … MUST equal" the INVITE's — so its Route set is reproduced
/// verbatim (load-bearing when the INVITE carried a preloaded outbound-proxy
/// Route: without it the CANCEL bypasses the proxy the INVITE traversed, and the
/// cross-message audit flags `cancelRouteEchoesInvite`). Panics when the INVITE
/// is missing a required header.
pub fn generate_cancel(invite_txn: &InviteClientTransactionHandle) -> SipRequest {
    let invite = &invite_txn.original_invite;
    let via = get_header(&invite.headers, "via").expect("generate_cancel: INVITE missing Via");
    let from = get_header(&invite.headers, "from").expect("generate_cancel: INVITE missing From");
    let to = get_header(&invite.headers, "to").expect("generate_cancel: INVITE missing To");
    let call_id =
        get_header(&invite.headers, "call-id").expect("generate_cancel: INVITE missing Call-ID");
    let invite_cseq = invite.cseq.seq;

    let mut headers: Vec<SipHeader> = vec![
        h("Via", via),
        h("Max-Forwards", "70"),
        h("From", from),
        h("To", to),
        h("Call-ID", call_id),
        h("CSeq", format!("{invite_cseq} CANCEL")),
    ];
    for route in get_headers(&invite.headers, "route") {
        headers.push(h("Route", route));
    }
    headers.push(h("Content-Length", "0"));

    make_request("CANCEL", &invite.uri, headers, Vec::new())
}
