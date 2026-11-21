use std::net::{TcpListener, UdpSocket};

use super::*;

/// `ports` handed out one per call, in order.
fn sequence(ports: &[u16]) -> impl FnMut() -> u16 + '_ {
    let mut next = ports.iter();
    move || *next.next().expect("the test supplies enough candidates")
}

/// A loopback port this test holds a TCP listener and a UDP socket on, so no pick can bind it.
fn taken() -> (u16, TcpListener, UdpSocket) {
    for _ in 0..TRIES {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a TCP listener");
        let port = tcp.local_addr().expect("its address").port();
        if let Ok(udp) = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)) {
            return (port, tcp, udp);
        }
    }
    panic!("no port free on both TCP and UDP");
}

#[test]
fn picks_one_distinct_loopback_address_per_kind_below_the_ephemeral_range() {
    let kinds = [Kind::Udp, Kind::Tcp, Kind::Tcp];
    let picked = free_addresses(&kinds).expect("three free addresses");
    let low = ephemeral_low().expect("the ephemeral range");
    assert_eq!(picked.len(), 3);
    for at in &picked {
        assert!(at.ip().is_loopback(), "{at} is on loopback");
        assert!(at.port() < low, "{at} lies below the ephemeral range, from {low}");
    }
    let ports: Vec<u16> = picked.iter().map(SocketAddr::port).collect();
    assert!(ports[1] != ports[2], "the two TCP addresses differ: {ports:?}");
}

#[test]
fn every_picked_port_is_released_when_the_pick_returns() {
    let kinds = [Kind::Udp, Kind::Tcp];
    let picked = free_addresses(&kinds).expect("two free addresses");
    UdpSocket::bind(picked[0]).expect("the UDP port binds again");
    TcpListener::bind(picked[1]).expect("the TCP port binds again");
}

#[test]
fn a_taken_port_is_skipped_for_the_next_candidate() {
    let (busy, _tcp, _udp) = taken();
    let free = free_addresses(&[Kind::Tcp, Kind::Tcp]).expect("two free ports");
    let candidates = [busy, free[0].port(), busy, free[1].port()];
    let picked = pick(&[Kind::Udp, Kind::Tcp], sequence(&candidates)).expect("the free ports");
    assert_eq!(
        picked.iter().map(SocketAddr::port).collect::<Vec<_>>(),
        [free[0].port(), free[1].port()]
    );
}

#[test]
fn a_port_an_earlier_address_took_is_not_picked_again() {
    let free = free_addresses(&[Kind::Tcp, Kind::Tcp]).expect("two free ports");
    let (first, second) = (free[0].port(), free[1].port());
    // A UDP and a TCP socket can share a port: only the repeat check refuses it.
    let picked =
        pick(&[Kind::Udp, Kind::Tcp], sequence(&[first, first, second])).expect("two ports");
    assert_eq!(picked.iter().map(SocketAddr::port).collect::<Vec<_>>(), [first, second]);
}

#[test]
fn a_pick_finding_no_free_port_fails_naming_its_kind() {
    let (busy, _tcp, _udp) = taken();
    let error = pick(&[Kind::Tcp], || busy).expect_err("the only candidate is taken");
    assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    assert!(error.to_string().contains("Tcp"), "{error}");
}

#[test]
fn the_range_starts_at_the_floor_or_at_the_first_unprivileged_port() {
    assert_eq!(range_below(32_768).expect("a range"), (FLOOR, 32_768));
    assert_eq!(range_below(FLOOR + SPAN).expect("a range"), (FLOOR, FLOOR + SPAN));
    assert_eq!(range_below(FLOOR + SPAN - 1).expect("a range"), (UNPRIVILEGED, FLOOR + SPAN - 1));
    assert_eq!(range_below(5000).expect("a range"), (UNPRIVILEGED, 5000));
    let error = range_below(UNPRIVILEGED + SPAN - 1).expect_err("too few ports below 2023");
    assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
}
