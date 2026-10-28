//! Where the router's own answer to a request goes ([`reply_to`]).

use std::net::SocketAddr;

use sip_message::received::response_destination;
use sip_message::SipRequest;

/// Where an answer to `req`, received from `src`, goes: the response target
/// of its top Via, which the transaction layer stamped on receipt (RFC 3261
/// §18.2.1, §18.2.2, RFC 3581 §4) — the address its server transaction sends
/// every response to. `src` only where that target names no IP address.
pub(super) fn reply_to(req: &SipRequest, src: SocketAddr) -> SocketAddr {
    response_destination(req.top_via()).unwrap_or(src)
}
