//! The host a checked URI names: the one its authority states, an IPv6
//! literal included, and the one a connection reaches.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use amqp_connect::{checked_uri, connect_uri, BrokerTls};
use tokio::time::Instant;

#[test]
fn a_bracketed_ipv6_host_is_the_uri_host_unbracketed() {
    let uri = checked_uri("VAR", "amqp://u:p@[2001:db8::1]:5673/%2f").unwrap();
    assert_eq!(uri.authority.host, "2001:db8::1");
    assert_eq!(uri.authority.port, 5673);
}

#[test]
fn an_ipv6_host_reads_in_its_rfc_5952_text() {
    let uri = checked_uri("VAR", "amqps://u:p@[2001:DB8:0:0::A]/%2f").unwrap();
    assert_eq!(uri.authority.host, "2001:db8::a");
    assert_eq!(uri.authority.port, 5671);
}

#[test]
fn an_ipv4_host_and_a_name_are_kept_as_written() {
    let ipv4 = checked_uri("VAR", "amqp://u:p@192.0.2.7:5672/%2f").unwrap();
    assert_eq!(ipv4.authority.host, "192.0.2.7");
    let name = checked_uri("VAR", "amqp://u:p@broker.example.test/%2f").unwrap();
    assert_eq!(name.authority.host, "broker.example.test");
}

#[test]
fn a_malformed_ipv6_host_is_refused_naming_the_variable() {
    let err = checked_uri("VAR", "amqp://u:p@[2001:db8::zz]:5672/%2f").unwrap_err();
    assert!(err.starts_with("VAR is not an AMQP URI"), "{err}");
}

/// The broker listens on 127.0.0.2 alone, which `localhost` never resolves
/// to, and the URI names it as the IPv4-mapped IPv6 address
/// `[::ffff:127.0.0.2]` (dual-stack sockets, Linux's default).
#[tokio::test]
async fn a_uri_with_an_ipv6_host_connects_to_that_address() {
    let listener = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let uri = checked_uri("VAR", &format!("amqp://g:g@[::ffff:127.0.0.2]:{port}/%2f")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let connected = connect_uri(&uri, &BrokerTls::PLAIN, deadline).await.unwrap();
    let broker: SocketAddr = format!("[::ffff:127.0.0.2]:{port}").parse().unwrap();
    assert_eq!(connected.socket.peer_addr().unwrap(), broker);
    assert!(listener.accept().is_ok());
}
