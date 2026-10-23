//! The broker stream: TCP to the first resolved address that answers, then
//! TLS for `amqps`, all by one deadline on the caller's task.

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

/// The stream the AMQP client's IO loop reads and writes.
pub type BrokerStream = AsyncTcpStream<<Tokio as Reactor>::TcpStream>;

/// A connected broker stream and a duplicate of its socket's descriptor:
/// shutting `socket` down ends the stream, whoever holds it.
pub struct Connected {
    pub stream: BrokerStream,
    pub socket: StdTcpStream,
}

pub(crate) fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connect deadline passed")
}

/// Connects the stream of `uri`, trying each of the resolved `addrs` in turn
/// until `deadline`; for `amqps`, the TLS handshake (SNI: the URI host) is
/// done by `deadline` too. No address is tried once `deadline` passed. A
/// failure or a dropped future leaves no socket open.
pub async fn connect_stream(
    uri: &AMQPUri,
    addrs: &[SocketAddr],
    deadline: Instant,
) -> io::Result<Connected> {
    let host = uri.authority.host.as_str();
    let runtime = Runtime::tokio_current();
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
    let tcp = connected.ok_or(last)?;
    let socket = StdTcpStream::from(tcp.get_ref().as_fd().try_clone_to_owned()?);
    let plain = AsyncTcpStream::Plain(tcp);
    let stream = match uri.scheme {
        AMQPScheme::AMQP => plain,
        AMQPScheme::AMQPS => timeout_at(deadline, plain.into_tls(host, TLSConfig::default()))
            .await
            .map_err(|_| timed_out())??,
    };
    Ok(Connected { stream, socket })
}
