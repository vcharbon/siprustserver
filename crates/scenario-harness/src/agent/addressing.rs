//! Wire-address resolution: the socket address a request/response physically
//! goes to next (route-set first hop, remote target, Via sent-by). Reading the
//! URI or the Via is the value model's job — this module only turns the
//! host:port it yields into a `SocketAddr`.

use std::net::SocketAddr;

use sip_message::generators::StackDialog;
use sip_message::header::{HostPort, NameAddr, Via};
use sip_message::sip_str::SipStr;
use sip_message::{SipRequest, SipResponse};

/// The `branch` parameter of a request's topmost Via — the transaction key
/// (RFC 3261 §8.1.1.7).
pub(crate) fn top_via_branch(req: &SipRequest) -> Option<String> {
    req.top_via().branch().map(str::to_string)
}

/// The `branch` of a response's topmost Via — the transaction the response
/// answers (RFC 3261 §17.1.3).
pub(crate) fn response_via_branch(resp: &SipResponse) -> Option<String> {
    resp.top_via().branch().map(str::to_string)
}

/// The socket address a sent-by / URI authority names (IPv4 fixtures only,
/// port defaulting to 5060 per RFC 3261 §19.1.2).
fn authority_to_addr(authority: &HostPort) -> Option<SocketAddr> {
    hostport_to_addr(&format!("{}:{}", authority.host(), authority.port_or_default()))
}

/// Resolve a SIP URI to a socket address. Handles `sip:user@host:port`, the
/// userless `<sip:host:port;lr>` name-addr form of a Route / Record-Route
/// entry, and a bare `host:port`.
pub(super) fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let t = uri.trim();
    // A bare `host[:port]` is not a URI — it resolves directly.
    if t.starts_with("sip:") || t.starts_with("sips:") || t.starts_with('<') {
        let addr = NameAddr::parse(&SipStr::owned(t)).ok()?;
        return authority_to_addr(addr.uri().authority());
    }
    hostport_to_addr(t)
}

/// Parse a bare `host:port` (or `host`, default port 5060) to a socket address.
pub(super) fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

/// The wire destination for an in-dialog request: the first hop in the route
/// set (the proxy) when present, else the dialog's remote target. For both
/// loose and strict routing the next hop is the address of `route_set[0]`'s
/// URI; with no route set it is the remote target.
pub(super) fn next_hop(dialog: &StackDialog, fallback: SocketAddr) -> SocketAddr {
    if let Some(top) = dialog.route_set.first() {
        if let Some(addr) = uri_to_addr(top) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The socket address a Via's sent-by names (RFC 3261 §18.2.2).
pub(super) fn via_addr(via: &Via) -> Option<SocketAddr> {
    authority_to_addr(via.sent_by())
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
pub(super) fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    via_addr(&req.top_via())
}
