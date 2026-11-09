//! The host the proxy's own OPTIONS senders state on their Via, From and
//! Contact.

use std::net::SocketAddr;

/// What a socket bound to the wildcard states: it knows no address of its own,
/// and an address of the proxy's service would draw the answer to either
/// instance. RFC 6761 §6.4 keeps `.invalid` from ever resolving, and the sender's
/// `;rport` has the answer sent to the packet's source (RFC 3581 §4).
const UNBOUND_HOST: &str = "proxy.invalid";

/// The host a sender on the socket bound at `local` states.
pub(super) fn stated_host(local: SocketAddr) -> String {
    if local.ip().is_unspecified() {
        UNBOUND_HOST.to_string()
    } else {
        local.ip().to_string()
    }
}
