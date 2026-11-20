//! The broker stream: TCP to the first resolved address that answers, each
//! address given its share of the time left, then TLS for `amqps`, all by one
//! deadline on the caller's task.

use std::io;
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::os::fd::AsFd;
use std::time::Duration;

use async_rs::traits::Reactor;
use async_rs::{Runtime, Tokio};
use lapin::{
    tcp::AsyncTcpStream,
    uri::{AMQPScheme, AMQPUri},
};
use tokio::time::{timeout_at, Instant};

use crate::tls::BrokerTls;

/// The stream the AMQP client's IO loop reads and writes.
pub type BrokerStream = AsyncTcpStream<<Tokio as Reactor>::TcpStream>;

/// A connected broker stream and a duplicate of its socket's descriptor:
/// shutting `socket` down ends the stream, whoever holds it.
pub struct Connected {
    pub stream: BrokerStream,
    pub socket: StdTcpStream,
}

/// The least time one try is given ([`try_by`]) while the deadline allows it,
/// so a long list does not cut every try below a connect's round trip.
pub const MIN_TRY: Duration = Duration::from_millis(250);

/// When one of `tries` still to make before `deadline` gives up: an even
/// share of the time left, at least [`MIN_TRY`], never past `deadline`. A try
/// that fails sooner leaves its unused time to the ones after it.
pub fn try_by(deadline: Instant, tries: usize) -> Instant {
    let now = Instant::now();
    let left = deadline.saturating_duration_since(now);
    let ways = u32::try_from(tries.max(1)).unwrap_or(u32::MAX);
    let share = left.checked_div(ways).unwrap_or(left).max(MIN_TRY).min(left);
    now + share
}

pub(crate) fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connect deadline passed")
}

/// Connects the stream of `uri`, trying each of the resolved `addrs` in turn
/// until `deadline`, each TCP connect bounded by its share of the time left
/// ([`try_by`]); for `amqps`, the TLS handshake (SNI: the URI host) over
/// `tls`'s connector is done by `deadline` too. An address that does not
/// answer within its share never keeps a later one from being tried. No
/// address is tried once `deadline` passed. A failure or a dropped future
/// leaves no socket open; an `amqps` URI with a plain `tls` is refused before
/// any connect.
pub async fn connect_stream(
    uri: &AMQPUri,
    tls: &BrokerTls,
    addrs: &[SocketAddr],
    deadline: Instant,
) -> io::Result<Connected> {
    connect_stream_sharing(uri, tls, addrs, 0, deadline).await
}

/// [`connect_stream`] for a caller that makes `later` more tries after these
/// addresses by the same `deadline`: each address's share counts them among
/// the tries left.
pub async fn connect_stream_sharing(
    uri: &AMQPUri,
    tls: &BrokerTls,
    addrs: &[SocketAddr],
    later: usize,
    deadline: Instant,
) -> io::Result<Connected> {
    let connector = match (&uri.scheme, tls.connector()) {
        (AMQPScheme::AMQP, _) => None,
        (AMQPScheme::AMQPS, Some(connector)) => Some(connector),
        (AMQPScheme::AMQPS, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an amqps URI needs its TLS connector (BrokerTls::for_uri)",
            ));
        }
    };
    let host = uri.authority.host.as_str();
    let runtime = Runtime::tokio_current();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} resolves to nothing"));
    let mut connected = None;
    for (tried, addr) in addrs.iter().enumerate() {
        if Instant::now() >= deadline {
            last = timed_out();
            break;
        }
        let by = try_by(deadline, addrs.len() - tried + later);
        match timeout_at(by, runtime.tcp_connect_addr(*addr)).await {
            Ok(Ok(s)) => {
                connected = Some(s);
                break;
            }
            Ok(Err(e)) => last = e,
            Err(_) => {
                last = io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{addr} did not answer within its share of the connect deadline"),
                );
            }
        }
    }
    let tcp = connected.ok_or(last)?;
    let socket = StdTcpStream::from(tcp.get_ref().as_fd().try_clone_to_owned()?);
    let plain = AsyncTcpStream::Plain(tcp);
    let stream = match connector {
        None => plain,
        Some(connector) => timeout_at(deadline, plain.into_rustls(connector, host))
            .await
            .map_err(|_| timed_out())??,
    };
    Ok(Connected { stream, socket })
}
