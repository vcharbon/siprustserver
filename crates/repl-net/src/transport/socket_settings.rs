//! [`SocketSettings`] — the kernel options every replication stream carries.
//!
//! A peer that vanishes without a reset (power loss, a black-holed route) is
//! detected by the kernel on both ends: on an idle socket by TCP keepalive, on
//! a socket with unacknowledged data by `TCP_USER_TIMEOUT`. The application
//! level bounds (the puller's idle timeout, the server's blocked-send bound)
//! do not depend on them; they close the socket sooner when the kernel knows.

use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;

/// TCP keepalive and user-timeout settings of a replication stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketSettings {
    /// Idle time before the first keepalive probe.
    pub keepalive_idle: Duration,
    /// Time between keepalive probes.
    pub keepalive_interval: Duration,
    /// Unanswered probes before the connection is dropped.
    pub keepalive_retries: u32,
    /// `TCP_USER_TIMEOUT`: the longest transmitted data may stay
    /// unacknowledged before the connection is dropped (Linux).
    pub user_timeout: Duration,
}

impl Default for SocketSettings {
    fn default() -> Self {
        Self {
            keepalive_idle: Duration::from_secs(5),
            keepalive_interval: Duration::from_secs(2),
            keepalive_retries: 3,
            user_timeout: Duration::from_secs(15),
        }
    }
}

impl SocketSettings {
    /// Set `TCP_NODELAY`, keepalive and the user timeout on `stream`. Every
    /// option is tried; the first one the kernel refuses is returned. The
    /// kernel counts keepalive times in whole seconds: a host validates them
    /// before they get here.
    pub(crate) fn apply(&self, stream: &TcpStream) -> std::io::Result<()> {
        let nodelay = stream.set_nodelay(true);
        let sock = SockRef::from(stream);
        let keepalive = TcpKeepalive::new()
            .with_time(self.keepalive_idle)
            .with_interval(self.keepalive_interval)
            .with_retries(self.keepalive_retries);
        let keepalive = sock.set_tcp_keepalive(&keepalive);
        #[cfg(target_os = "linux")]
        let user_timeout = sock.set_tcp_user_timeout(Some(self.user_timeout));
        #[cfg(not(target_os = "linux"))]
        let user_timeout = Ok(());
        nodelay.and(keepalive).and(user_timeout)
    }
}
