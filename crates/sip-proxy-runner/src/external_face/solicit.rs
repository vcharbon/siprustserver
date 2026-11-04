//! Asking the kernel to resolve a next hop, with no capability: one empty
//! datagram toward it from a fresh unbound UDP socket. The kernel resolves the
//! neighbour to send it, whatever the entry's state. The route guard's chain
//! drops only BFD control (UDP 3784) and BGP (TCP 179) toward the routers, so
//! this datagram reaches the neighbour layer while the node is withdrawn.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// The discard service (RFC 863): a router that receives the datagram has
/// nothing to do with it.
const DISCARD_PORT: u16 = 9;

/// Send the datagram that makes the kernel resolve `hop`. A failure leaves the
/// reading as it stands: the next one solicits again.
pub fn solicit(hop: Ipv4Addr) {
    if let Err(e) = send_empty(SocketAddrV4::new(hop, DISCARD_PORT)) {
        tracing::debug!(%hop, error = %e, "next hop solicit not sent");
    }
}

fn send_empty(to: SocketAddrV4) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.send_to(&[], to).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solicit_is_one_empty_datagram() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_millis(900))).unwrap();
        let std::net::SocketAddr::V4(to) = receiver.local_addr().unwrap() else {
            panic!("bound to IPv4");
        };
        send_empty(to).unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(n, 0);
    }
}
