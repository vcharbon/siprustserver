//! Connecting by URI: the host resolved, then the stream connected, both by
//! one deadline.

use std::io;
use std::net::SocketAddr;

use lapin::uri::AMQPUri;
use tokio::time::{timeout_at, Instant};

use crate::stream::{connect_stream, timed_out, Connected};

/// Resolves the host of `uri` and connects its stream ([`connect_stream`]),
/// both by `deadline`.
pub async fn connect_uri(uri: &AMQPUri, deadline: Instant) -> io::Result<Connected> {
    let host = (uri.authority.host.as_str(), uri.authority.port);
    let addrs: Vec<SocketAddr> = timeout_at(deadline, tokio::net::lookup_host(host))
        .await
        .map_err(|_| timed_out())??
        .collect();
    connect_stream(uri, &addrs, deadline).await
}
