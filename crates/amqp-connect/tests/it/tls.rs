//! `amqps`: the stream is TLS from its first byte, names the URI host in its
//! ClientHello, every handshake rides the one connector built for the URI,
//! and a handshake the peer never answers ends at the deadline with the
//! socket closed.

use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use amqp_connect::{connect_stream, BrokerTls};
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

/// A fixture CA, trusted nowhere: the one root these tests' connectors read.
const TEST_CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-ca.pem");
/// A CA file holding no certificate: no connector builds over it.
const NO_CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/no-ca.pem");

/// The variables that name the CA roots a connector reads: a file, and
/// directories read beside it.
const CA_VARS: [&str; 2] = ["SSL_CERT_FILE", "SSL_CERT_DIR"];

/// The CA roots a connector reads, `path` alone, until dropped; then both
/// variables as they were. Each test runs in a process of its own (nextest)
/// and sets them before starting any thread.
struct CaFile([Option<OsString>; 2]);

impl CaFile {
    fn at(path: &str) -> Self {
        let before = CA_VARS.map(std::env::var_os);
        std::env::set_var("SSL_CERT_FILE", path);
        std::env::remove_var("SSL_CERT_DIR");
        Self(before)
    }
}

impl Drop for CaFile {
    fn drop(&mut self) {
        for (var, before) in CA_VARS.iter().zip(&mut self.0) {
            match before.take() {
                Some(before) => std::env::set_var(var, before),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// The TLS of an `amqps` URI: its connector, built once over the fixture CA.
fn amqps_tls() -> BrokerTls {
    let _ca = CaFile::at(TEST_CA);
    BrokerTls::for_uri(&amqps_uri(5671)).expect("the fixture CA loads")
}

/// The hello one `amqps` connect over `tls` sends.
async fn hello_over(tls: &BrokerTls) -> Hello {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = read_one_hello(listener);
    let deadline = Instant::now() + Duration::from_secs(5);
    let refused = connect_stream(&amqps_uri(addr.port()), tls, &[addr], deadline).await;
    assert!(refused.is_err(), "a peer that hangs up completes no handshake");
    peer.join().unwrap()
}

async fn hello_of_an_amqps_connect() -> Hello {
    hello_over(&amqps_tls()).await
}

/// Once the connector is built no root loads any more, and both handshakes
/// still send their hello: neither builds a connector of its own.
#[tokio::test]
async fn two_handshakes_ride_one_connector_built_once() {
    let tls = amqps_tls();
    let _no_ca = CaFile::at(NO_CA);
    assert!(BrokerTls::for_uri(&amqps_uri(5671)).is_err(), "no connector builds from here on");
    for _ in 0..2 {
        let hello = hello_over(&tls).await;
        assert_eq!(hello.first_byte, 0x16, "a TLS handshake record");
        assert_eq!(hello.server_name.as_deref(), Some(HOST));
    }
}

#[tokio::test]
async fn an_amqps_uri_with_no_connector_is_refused_before_any_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let uri = amqps_uri(addr.port());
    let err = connect_stream(&uri, &BrokerTls::PLAIN, &[addr], deadline).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::InvalidInput));
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "nothing connected");
}

#[test]
fn an_amqp_uri_runs_no_tls() {
    let uri: AMQPUri = "amqp://g:g@broker.amqp.test:5672/%2f".parse().unwrap();
    assert!(!BrokerTls::for_uri(&uri).unwrap().is_tls());
    assert!(amqps_tls().is_tls());
}

#[test]
fn an_amqps_uri_whose_roots_do_not_load_builds_no_connector() {
    let _no_ca = CaFile::at(NO_CA);
    assert!(BrokerTls::for_uri(&amqps_uri(5671)).is_err());
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
    let tls = amqps_tls();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(300);
    let err = connect_stream(&amqps_uri(addr.port()), &tls, &[addr], deadline).await.err();
    assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
    assert!(t0.elapsed() < Duration::from_millis(600), "returned {:?} after", t0.elapsed());
    assert_eq!(peer.join().unwrap().ok(), Some(0), "the peer sees the socket close");
}
