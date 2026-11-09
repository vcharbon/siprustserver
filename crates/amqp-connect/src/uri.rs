//! Connecting by URI: the host resolved, then the stream connected, both by
//! one deadline.

use std::io;
use std::net::SocketAddr;

use lapin::uri::AMQPUri;
use tokio::time::{timeout_at, Instant};

use crate::stream::{connect_stream, timed_out, Connected};
use crate::tls::BrokerTls;

/// The heartbeat, seconds, a connection whose URI states none negotiates
/// ([`open_over`](crate::open_over)): a silent broker or a half-open link is
/// read dead within two of them.
pub const DEFAULT_HEARTBEAT_SEC: u16 = 30;

/// `url`, the value of the variable `var`, as the URI a connection opens:
/// `Err` naming `var` when it does not parse or states `heartbeat=0`, which
/// leaves a silent peer undetected once the link is up.
pub fn checked_uri(var: &str, url: &str) -> Result<AMQPUri, String> {
    let uri: AMQPUri = url.parse().map_err(|e| format!("{var} is not an AMQP URI: {e}"))?;
    if uri.query.heartbeat == Some(0) {
        return Err(format!(
            "{var} states heartbeat=0: a silent broker would hold the link forever; state a \
             positive heartbeat or none ({DEFAULT_HEARTBEAT_SEC} s)"
        ));
    }
    Ok(uri)
}

/// Resolves the host of `uri` and connects its stream over `tls`
/// ([`connect_stream`]), both by `deadline`.
pub async fn connect_uri(
    uri: &AMQPUri,
    tls: &BrokerTls,
    deadline: Instant,
) -> io::Result<Connected> {
    let host = (uri.authority.host.as_str(), uri.authority.port);
    let addrs: Vec<SocketAddr> = timeout_at(deadline, tokio::net::lookup_host(host))
        .await
        .map_err(|_| timed_out())??
        .collect();
    connect_stream(uri, tls, &addrs, deadline).await
}
