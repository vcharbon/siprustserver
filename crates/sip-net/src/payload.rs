//! The largest payload one UDP datagram carries, per address family. A send
//! past it is refused by the socket (`EMSGSIZE`).

use std::net::SocketAddr;

/// The largest UDP payload over IPv4 (65535 − 20 IP − 8 UDP). SIP fragments
/// far below it.
pub const MAX_UDP_PAYLOAD: usize = 65507;

/// The largest UDP payload over IPv6 with no jumbogram (65535 payload length −
/// 8 UDP).
pub const MAX_UDP6_PAYLOAD: usize = 65527;

/// The largest payload one UDP datagram to `dst` carries.
pub fn max_udp_payload(dst: SocketAddr) -> usize {
    match dst {
        SocketAddr::V4(_) => MAX_UDP_PAYLOAD,
        SocketAddr::V6(_) => MAX_UDP6_PAYLOAD,
    }
}
