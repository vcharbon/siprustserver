//! Asking the kernel to resolve a next hop, with no capability: one empty
//! datagram toward it from a fresh unbound UDP socket. The kernel resolves the
//! neighbour to send it, whatever the entry's state. The route guard's chain
//! drops only BFD control (UDP 3784) and BGP (TCP 179) toward the routers, so
//! this datagram reaches the neighbour layer while the node is withdrawn. A
//! node firewall that drops it at output fails the send (`EPERM`).

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// The discard service (RFC 863): a router that receives the datagram has
/// nothing to do with it.
pub(super) const DISCARD_PORT: u16 = 9;

/// Send the datagram that makes the kernel resolve `hop`.
pub(super) fn solicit(hop: Ipv4Addr, port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.send_to(&[], SocketAddrV4::new(hop, port)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solicit_is_one_empty_datagram() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_millis(900))).unwrap();
        solicit(Ipv4Addr::LOCALHOST, receiver.local_addr().unwrap().port()).unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(n, 0);
    }
}
