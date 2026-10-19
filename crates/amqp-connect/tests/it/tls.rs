//! `amqps`: the stream is TLS from its first byte, names the URI host in its
//! ClientHello, and a handshake the peer never answers ends at the deadline
//! with the socket closed.

use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use amqp_connect::connect_stream;
use lapin::uri::AMQPUri;
use rustls::server::Acceptor;
use tokio::time::Instant;

const HOST: &str = "broker.amqp.test";

fn amqps_uri(port: u16) -> AMQPUri {
    format!("amqps://g:g@{HOST}:{port}/%2f").parse().unwrap()
}

/// What a peer read off one accepted connection before it hung up.
struct Hello {
    first_byte: u8,
    server_name: Option<String>,
}

/// Accepts one connection, reads its ClientHello and hangs up.
fn read_one_hello(listener: TcpListener) -> JoinHandle<Hello> {
    std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut first = [0u8; 1];
        assert_eq!(peer.peek(&mut first).unwrap(), 1);
        let mut acceptor = Acceptor::default();
        let accepted = loop {
            acceptor.read_tls(&mut peer).unwrap();
            if let Some(accepted) = acceptor.accept().map_err(|(e, _)| e).unwrap() {
                break accepted;
            }
        };
        let server_name = accepted.client_hello().server_name().map(str::to_owned);
        Hello { first_byte: first[0], server_name }
    })
}

async fn hello_of_an_amqps_connect() -> Hello {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = read_one_hello(listener);
    let deadline = Instant::now() + Duration::from_secs(5);
    let refused = connect_stream(&amqps_uri(addr.port()), &[addr], deadline).await;
    assert!(refused.is_err(), "a peer that hangs up completes no handshake");
    peer.join().unwrap()
}

#[tokio::test]
async fn an_amqps_stream_opens_with_a_tls_record_not_the_amqp_header() {
    let hello = hello_of_an_amqps_connect().await;
    assert_eq!(hello.first_byte, 0x16, "a TLS handshake record, not `AMQP`");
}

#[tokio::test]
async fn an_amqps_client_hello_names_the_uri_host() {
    let hello = hello_of_an_amqps_connect().await;
    assert_eq!(hello.server_name.as_deref(), Some(HOST));
}

/// Reads until the client ends the connection; `Ok(0)` is its close.
fn read_to_close(mut peer: TcpStream) -> io::Result<usize> {
    peer.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 4096];
    loop {
        match peer.read(&mut buf)? {
            0 => return Ok(0),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn an_unanswered_client_hello_times_out_at_the_deadline_and_closes_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let peer = std::thread::spawn(move || read_to_close(listener.accept().unwrap().0));
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(300);
    let err = connect_stream(&amqps_uri(addr.port()), &[addr], deadline).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
    assert!(t0.elapsed() < Duration::from_millis(600), "returned {:?} after", t0.elapsed());
    assert_eq!(peer.join().unwrap().ok(), Some(0), "the peer sees the socket close");
}
