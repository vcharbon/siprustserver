//! The receive pump over a real socket.

use super::*;

/// A socket connected to a port nobody listens on: its first send draws an
/// ICMP port-unreachable, which the kernel reports as `ECONNREFUSED` on the
/// socket's next receive. On loopback the ICMP is handled before `send`
/// returns, so the error is pending once it has. `peer` is the closed port's
/// address, free to bind afterwards.
async fn refused_socket() -> (UdpSocket, SocketAddr) {
    let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer = closed.local_addr().unwrap();
    drop(closed);
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(peer).await.unwrap();
    socket.send(b"to nobody").await.unwrap();
    (socket, peer)
}

#[tokio::test]
async fn a_receive_error_does_not_stop_the_pump() {
    let (socket, peer) = refused_socket().await;
    let socket = Arc::new(socket);
    let queue = Arc::new(PacketQueue::new(8));
    let counters = Arc::new(Counters::default());
    let _pump = tokio::spawn(recv_loop(
        socket.clone(),
        queue.clone(),
        counters.clone(),
        None,
        Clock::test_at(0),
    ));

    let sender = UdpSocket::bind(peer).await.unwrap();
    sender.send_to(b"after the error", socket.local_addr().unwrap()).await.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(5), queue.take()).await;
    let raw = received.ok().flatten().map(|pkt| pkt.raw);
    assert_eq!(raw.as_deref(), Some(&b"after the error"[..]), "the pump stopped on the error");
    assert_eq!(counters.snapshot().recv_errors, 1);
}

#[test]
fn an_error_of_one_datagram_reads_on_at_once_and_exhaustion_waits() {
    use std::io::{Error, ErrorKind};
    for kind in [
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::HostUnreachable,
        ErrorKind::NetworkUnreachable,
        ErrorKind::Interrupted,
    ] {
        assert_eq!(recv_pause(&Error::from(kind)), None, "{kind:?}");
    }
    // ENOMEM, ENOBUFS.
    for errno in [libc::ENOMEM, libc::ENOBUFS] {
        assert_eq!(recv_pause(&Error::from_raw_os_error(errno)), Some(RECV_PAUSE), "{errno}");
    }
}
