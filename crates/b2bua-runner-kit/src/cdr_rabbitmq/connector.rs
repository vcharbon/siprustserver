//! The socket connector of the RabbitMQ CDR sink's connections: the TCP
//! connect runs here, under the connect deadline, so the connection's socket is
//! armed on its [`SocketKill`] before the AMQP handshake starts.

use std::io;
use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::time::Instant;

use lapin::{
    tcp::{HandshakeResult, TLSConfig, TcpStream},
    uri::{AMQPScheme, AMQPUri},
};

use super::socket_kill::SocketKill;

/// Connects the socket of `uri`, trying each resolved address in turn until
/// `deadline`, arms `kill` with it, then wraps it as the AMQP client expects:
/// TLS for `amqps`, non-blocking.
#[allow(clippy::result_large_err)] // the AMQP client's connector contract
pub(super) fn connect_socket(
    uri: &AMQPUri,
    deadline: Instant,
    kill: &SocketKill,
) -> HandshakeResult {
    let host = uri.authority.host.as_str();
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "connect deadline passed");
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} resolves to nothing"));
    let mut connected = None;
    for addr in (host, uri.authority.port).to_socket_addrs()? {
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
        let Some(left) = left else {
            last = timed_out();
            break;
        };
        match StdTcpStream::connect_timeout(&addr, left) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let socket = connected.ok_or(last)?;
    socket.set_nodelay(true)?;
    kill.arm(socket.try_clone()?);
    let stream = TcpStream::from_std(socket)?;
    let stream = match uri.scheme {
        AMQPScheme::AMQP => stream,
        AMQPScheme::AMQPS => stream.into_tls(host, TLSConfig::default())?,
    };
    stream.set_nonblocking(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_passed_deadline_connects_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let uri: AMQPUri =
            format!("amqp://g:g@{}/%2f", listener.local_addr().unwrap()).parse().unwrap();
        let kill = SocketKill::default();
        let past = Instant::now() - Duration::from_millis(1);
        let err = connect_socket(&uri, past, &kill).expect_err("no time left");
        assert!(format!("{err:?}").contains("TimedOut"), "{err:?}");
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no connection may be attempted past the deadline");
    }
}
