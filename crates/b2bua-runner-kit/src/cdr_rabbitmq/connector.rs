//! The socket connector of the RabbitMQ CDR sink's connections. The AMQP
//! client's IO loop calls it once per connection (auto-recovery is off): the
//! TCP connect to the resolved broker addresses and the TLS handshake run
//! under the connect deadline, and the socket is armed on its [`SocketKill`]
//! before the AMQP handshake starts.

use std::io;
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::os::fd::AsFd;

use async_rs::traits::Reactor;
use async_rs::{Runtime, Tokio};
use lapin::{
    tcp::{AsyncTcpStream, TLSConfig},
    uri::{AMQPScheme, AMQPUri},
};
use tokio::time::{timeout_at, Instant};

use super::socket_kill::SocketKill;

/// The broker stream the AMQP client's IO loop reads and writes.
pub(super) type BrokerStream = AsyncTcpStream<<Tokio as Reactor>::TcpStream>;

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connect deadline passed")
}

/// Connects the socket of `uri` on `runtime`'s reactor, trying each of its
/// resolved `addrs` in turn until `deadline`, arms `kill` with it, then wraps
/// it as the AMQP client expects: TLS for `amqps`, its handshake done by
/// `deadline` too. No address is tried once `deadline` passed.
pub(super) async fn connect_socket(
    uri: &AMQPUri,
    addrs: &[SocketAddr],
    deadline: Instant,
    kill: &SocketKill,
    runtime: &Runtime<Tokio>,
) -> io::Result<BrokerStream> {
    let host = uri.authority.host.as_str();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} resolves to nothing"));
    let mut connected = None;
    for addr in addrs {
        if Instant::now() >= deadline {
            last = timed_out();
            break;
        }
        match timeout_at(deadline, runtime.tcp_connect_addr(*addr)).await {
            Ok(Ok(s)) => {
                connected = Some(s);
                break;
            }
            Ok(Err(e)) => last = e,
            Err(_) => {
                last = timed_out();
                break;
            }
        }
    }
    let socket = connected.ok_or(last)?;
    kill.arm(StdTcpStream::from(socket.get_ref().as_fd().try_clone_to_owned()?));
    let stream = AsyncTcpStream::Plain(socket);
    match uri.scheme {
        AMQPScheme::AMQP => Ok(stream),
        AMQPScheme::AMQPS => timeout_at(deadline, stream.into_tls(host, TLSConfig::default()))
            .await
            .map_err(|_| timed_out())?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    fn uri_of(addr: SocketAddr) -> AMQPUri {
        format!("amqp://g:g@{addr}/%2f").parse().unwrap()
    }

    #[tokio::test]
    async fn a_passed_deadline_connects_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let kill = SocketKill::default();
        let past = Instant::now() - Duration::from_millis(1);
        let rt = Runtime::tokio_current();
        let err = connect_socket(&uri_of(addr), &[addr], past, &kill, &rt).await.err();
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no connection may be attempted past the deadline");
    }

    #[tokio::test]
    async fn the_connected_socket_is_armed_on_its_kill() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let kill = SocketKill::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        let rt = Runtime::tokio_current();
        let stream = connect_socket(&uri_of(addr), &[addr], deadline, &kill, &rt).await.unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        kill.kill();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(matches!(peer.read(&mut [0u8; 1]), Ok(0)), "the kill ends the client's socket");
        drop(stream);
    }
}
