//! The plain TCP connect: bounded by its deadline, and its descriptor ends
//! the stream it duplicates.

use std::io::{self, Read};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::time::Duration;

use amqp_connect::{connect_stream, connect_uri};
use lapin::uri::AMQPUri;
use tokio::time::Instant;

fn uri_of(addr: SocketAddr) -> AMQPUri {
    format!("amqp://g:g@{addr}/%2f").parse().unwrap()
}

#[tokio::test]
async fn a_passed_deadline_connects_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let past = Instant::now() - Duration::from_millis(1);
    let err = connect_stream(&uri_of(addr), &[addr], past).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "no connection may be attempted past the deadline");
}

#[tokio::test]
async fn a_refused_connect_is_an_error() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let deadline = Instant::now() + Duration::from_secs(5);
    let err = connect_stream(&uri_of(addr), &[addr], deadline).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::ConnectionRefused));
}

#[tokio::test]
async fn shutting_the_descriptor_down_ends_the_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let connected = connect_stream(&uri_of(addr), &[addr], deadline).await.unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    connected.socket.shutdown(Shutdown::Both).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert!(matches!(peer.read(&mut [0u8; 1]), Ok(0)), "the peer sees the stream end");
    drop(connected.stream);
}

#[tokio::test]
async fn a_uri_connects_to_its_resolved_host() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let uri: AMQPUri = format!("amqp://g:g@localhost:{port}/%2f").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let connected = connect_uri(&uri, deadline).await.unwrap();
    assert!(listener.accept().is_ok());
    drop(connected);
}
