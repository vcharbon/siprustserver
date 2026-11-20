//! The plain TCP connect: bounded by its deadline, and its descriptor ends
//! the stream it duplicates.

use std::io::{self, Read};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::time::Duration;

use amqp_connect::{connect_stream, connect_uri, BrokerTls};
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
    let err = connect_stream(&uri_of(addr), &BrokerTls::PLAIN, &[addr], past).await.err();
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
    let err = connect_stream(&uri_of(addr), &BrokerTls::PLAIN, &[addr], deadline).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::ConnectionRefused));
}

/// A listener whose zero backlog one connection fills: the kernel drops every
/// further SYN, so a connect to it hangs until its own bound.
fn black_holed() -> (socket2::Socket, std::net::TcpStream, SocketAddr) {
    use socket2::{Domain, Socket, Type};
    let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    listener.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
    listener.listen(0).unwrap();
    let addr = listener.local_addr().unwrap().as_socket().unwrap();
    let filler = std::net::TcpStream::connect(addr).unwrap();
    let blocked = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100));
    assert!(blocked.is_err(), "the full backlog must drop a further SYN");
    (listener, filler, addr)
}

/// The first address never answers: it is given its share of the deadline,
/// not all of it, and the second address is connected well before the
/// deadline.
#[tokio::test]
async fn an_address_that_never_answers_leaves_the_next_one_its_share() {
    let (_held, _filler, silent) = black_holed();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let answering = listener.local_addr().unwrap();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(700);
    let addrs = [silent, answering];
    let connected = connect_stream(&uri_of(answering), &BrokerTls::PLAIN, &addrs, deadline).await;
    let took = t0.elapsed();
    assert!(connected.is_ok(), "the answering address connects: {:?}", connected.err());
    assert!(listener.accept().is_ok());
    assert!(
        took >= Duration::from_millis(340) && took < Duration::from_millis(600),
        "the silent address held the connect {took:?}: its share is half the deadline"
    );
}

/// An address refused at once hands its unused share to the next one, which
/// is given the whole time left.
#[tokio::test]
async fn a_refused_address_hands_its_share_to_the_next_one() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let refusing = closed.local_addr().unwrap();
    drop(closed);
    let (_held, _filler, silent) = black_holed();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(500);
    let addrs = [refusing, silent];
    let err = connect_stream(&uri_of(silent), &BrokerTls::PLAIN, &addrs, deadline).await.err();
    let took = t0.elapsed();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
    assert!(took >= Duration::from_millis(480), "the silent address had {took:?}, not the rest");
}

#[tokio::test]
async fn shutting_the_descriptor_down_ends_the_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let tls = BrokerTls::PLAIN;
    let connected = connect_stream(&uri_of(addr), &tls, &[addr], deadline).await.unwrap();
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
    let connected = connect_uri(&uri, &BrokerTls::PLAIN, deadline).await.unwrap();
    assert!(listener.accept().is_ok());
    drop(connected);
}
