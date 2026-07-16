//! Wire-address resolution: the socket address a request/response physically
//! goes to next (route-set first hop, remote target, Via sent-by). All SIP
//! header/URI *parsing* is delegated to `sip_message::message_helpers` — no
//! extraction lives here (sip-message is its only sanctioned home).

use std::net::SocketAddr;

use sip_message::generators::{strip_route_uri_to_request_uri, StackDialog};
use sip_message::message_helpers::{
    extract_host_port, get_header, parse_via_params, via_sent_by,
};
use sip_message::{SipHeader, SipRequest};

/// The `branch` parameter of the topmost Via header, if any.
pub(crate) fn top_via_branch(headers: &[SipHeader]) -> Option<String> {
    parse_via_params(get_header(headers, "via")?).branch
}

/// Resolve a SIP URI to a socket address (default port 5060, IPv4 fixtures
/// only). Handles `sip:user@host:port`, the userless `sip:host:port;lr` form
/// of a Route/Record-Route URI, and a bare `host:port`.
pub(super) fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let t = uri.trim();
    // `parse_sip_uri` needs a scheme (it reads everything before the first
    // ':' as one); a bare `host[:port]` is resolved directly.
    if t.starts_with("sip:") || t.starts_with("sips:") || t.starts_with('<') {
        let (host, port) = extract_host_port(t)?;
        return hostport_to_addr(&format!("{host}:{port}"));
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
        if let Some(addr) = uri_to_addr(&strip_route_uri_to_request_uri(top)) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The socket address in a Via value's sent-by (RFC 3261 §18.2.2).
pub(super) fn via_addr(via_value: &str) -> Option<SocketAddr> {
    let (host, port) = via_sent_by(via_value)?;
    hostport_to_addr(&format!("{host}:{port}"))
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
pub(super) fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    via_addr(get_header(&req.headers, "via")?)
}
