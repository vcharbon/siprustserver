//! The proxy's debug server (`PROXY_DEBUG`): `/debug/flamegraph` on a
//! loopback listener of its own. A profiling aid only: the proxy serves
//! without it when its bind fails.

use std::net::SocketAddr;

use probe_http::{AdminRoutes, AdminServer};

/// The debug server on `addr`, bound once with no retry; hold it for the
/// process lifetime. A bind failure is logged and gives `None`.
pub async fn start(addr: SocketAddr) -> Option<AdminServer> {
    match AdminServer::start(addr, AdminRoutes::default()).await {
        Ok(server) => {
            tracing::info!(addr = %server.addr(), "debug server listening (/debug/flamegraph)");
            Some(server)
        }
        Err(e) => {
            tracing::warn!(%addr, error = %e, "debug server failed to bind");
            None
        }
    }
}

#[cfg(test)]
mod tests;
