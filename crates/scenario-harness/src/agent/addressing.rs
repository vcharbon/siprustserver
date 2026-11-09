//! Wire-address resolution: the socket address a request/response physically
//! goes to next (route-set first hop, remote target, Via sent-by). Reading the
//! URI or the Via is the value model's job — this module only turns the
//! host:port it yields into a `SocketAddr`.

use std::net::SocketAddr;

use sip_message::generators::StackDialog;
use sip_message::header::{HostPort, NameAddr, Uri, Via};
use sip_message::sip_str::SipStr;
use sip_message::SipRequest;

/// The `branch` parameter of a request's topmost Via — the transaction key
/// (RFC 3261 §8.1.1.7).
pub(crate) fn top_via_branch(req: &SipRequest) -> Option<String> {
    req.top_via().branch().map(str::to_string)
}

/// The socket address a sent-by / URI authority names (IPv4 fixtures only,
/// port defaulting to 5060 per RFC 3261 §19.1.2).
fn authority_to_addr(authority: &HostPort) -> Option<SocketAddr> {
    hostport_to_addr(&format!("{}:{}", authority.host(), authority.port_or_default()))
}

/// The socket address a URI's next hop names ([`Uri::next_hop`]).
fn next_hop_addr(uri: &Uri) -> Option<SocketAddr> {
    let (host, port) = uri.next_hop();
    hostport_to_addr(&format!("{host}:{port}"))
}

/// Resolve a SIP URI to a socket address. Handles a bare `sip:user@host:port`
/// URI read whole (a `;` in its user part stays there, RFC 3261 §25.1), the
/// `<sip:host:port;lr>` name-addr form of a Route / Record-Route entry, and a
/// bare `host:port`.
pub(super) fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let t = uri.trim();
    if t.starts_with("sip:") || t.starts_with("sips:") {
        let uri = Uri::parse(&SipStr::owned(t)).ok()?;
        return next_hop_addr(&uri);
    }
    if t.starts_with('<') {
        let addr = NameAddr::parse(&SipStr::owned(t)).ok()?;
        return next_hop_addr(addr.uri());
    }
    // A bare `host[:port]` is not a URI — it resolves directly.
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
    via_addr(req.top_via())
}

#[cfg(test)]
mod tests {
    use super::uri_to_addr;

    /// A dialog's remote target is a URI read whole: a `;` in its user part
    /// (RFC 3261 §25.1 `user-unreserved`) stays in the user part, and the
    /// address is the host and port after the `@`.
    #[test]
    fn a_remote_target_whose_user_part_holds_a_parameter_names_its_socket() {
        for target in [
            "sip:+33612345678;foo=bar@192.0.2.10:5070",
            "sip:alice%3Bx=y;isub=12@192.0.2.10:5070;transport=udp",
            "<sip:+33612345678;foo=bar@192.0.2.10:5070;lr>",
            "192.0.2.10:5070",
            "sip:bob@name.invalid:5070;maddr=192.0.2.10",
            "<sip:name.invalid:5070;maddr=192.0.2.10;lr>",
        ] {
            assert_eq!(uri_to_addr(target), Some("192.0.2.10:5070".parse().unwrap()), "{target}");
        }
    }
}
