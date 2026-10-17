//! The real server's listening socket: how it is bound and how its accept
//! errors are paced.

use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpSocket};

/// The listen backlog asked of the kernel, which clamps it to `somaxconn`.
pub(crate) const BACKLOG: u32 = 4096;

/// How long the accept loop waits after an error that is not one connection's.
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// A listener bound at `addr` with [`BACKLOG`], the address reusable as tokio's
/// own `TcpListener::bind` makes it.
pub(crate) fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(BACKLOG)
}

/// How long to wait before the next accept after `error`: nothing when it was
/// one connection's own failure, [`ACCEPT_BACKOFF`] when the process or the
/// kernel is out of a resource (descriptors, buffers, memory), which an
/// immediate retry cannot have freed.
pub(crate) fn accept_backoff(error: &io::Error) -> Option<Duration> {
    match error.kind() {
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted => None,
        _ => Some(ACCEPT_BACKOFF),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_failure_retries_at_once_and_exhaustion_waits() {
        for kind in
            [ErrorKind::ConnectionAborted, ErrorKind::ConnectionReset, ErrorKind::Interrupted]
        {
            assert_eq!(accept_backoff(&io::Error::from(kind)), None, "{kind:?}");
        }
        // EMFILE, ENFILE, ENOBUFS, ENOMEM.
        for errno in [24, 23, 105, 12] {
            assert_eq!(
                accept_backoff(&io::Error::from_raw_os_error(errno)),
                Some(ACCEPT_BACKOFF),
                "errno {errno}"
            );
        }
    }

    #[tokio::test]
    async fn binds_v4_and_v6_with_an_ephemeral_port() {
        let v4 = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_ne!(v4.local_addr().unwrap().port(), 0);
        // A host without IPv6 loopback refuses the bind; that is not this test's subject.
        if let Ok(v6) = bind("[::1]:0".parse().unwrap()) {
            assert!(v6.local_addr().unwrap().is_ipv6());
        }
    }
}
