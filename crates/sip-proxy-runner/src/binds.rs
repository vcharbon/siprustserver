//! Every socket the proxy serves on, bound in order before it serves. Every
//! bind rides the same bounded EADDRINUSE retry, and ANY bind still failing
//! at the deadline refuses the boot — it never runs with a subset of its
//! ports.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::socket_stats::was_clamped;
use sip_net::types::BindUdpOpts;
use sip_net::{BindError, RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use sip_proxy::intake_shed::{intake_shed_hook, intake_shed_watermark};
use sip_proxy::ProxyMetrics;

use crate::config::{ExternalFace, ProxyConfig};

/// The proxy's sockets.
pub struct Sockets {
    /// The metrics/probe HTTP listener, its address read; its accept loop
    /// starts later.
    pub metrics: tokio::net::TcpListener,
    /// The signalling endpoints of each recv shard.
    pub shards: Vec<ShardSockets>,
    /// The OPTIONS health probe's own source socket.
    pub probe: Box<dyn UdpEndpoint>,
}

/// One recv shard's signalling endpoints.
pub struct ShardSockets {
    /// Its internal-face endpoint.
    pub internal: Box<dyn UdpEndpoint>,
    /// Its external-face endpoint: one exactly when the proxy has an
    /// external face.
    pub external: Option<Box<dyn UdpEndpoint>>,
}

/// Bind every socket `cfg` names, each signalling endpoint stamping arrivals
/// on `clock`.
///
/// BIND ORDER: metrics/probe HTTP listener FIRST, then the internal-face
/// signaling sockets, then the external-face sockets LAST. The SIP stack is
/// never exposed unless the process can already report health, and the
/// metrics port — inherently exclusive TCP — is the node-singleton lock:
/// holding it proves no other proxy instance is alive on the node in every
/// shard config, whereas the data sockets may carry SO_REUSEPORT
/// (recv_shards > 1) and would silently overlap a predecessor. The accept
/// loop starts later (ProbeServer::serve_on) — the bind is the lock.
///
/// `Err` names the first socket that did not bind: the boot stops on it.
/// `proxy_metrics` counts what the signaling sockets' intake hook drops.
pub async fn bind_sockets(
    cfg: &ProxyConfig,
    clock: &Clock,
    proxy_metrics: &Arc<ProxyMetrics>,
) -> Result<Sockets, String> {
    let net = RealSignalingNetwork::new();
    let (listen_sa, metrics_sa, queue_max) = (cfg.listen, cfg.metrics, cfg.queue_max);
    let (udp_sndbuf, udp_rcvbuf, recv_shards) = (cfg.udp_sndbuf, cfg.udp_rcvbuf, cfg.recv_shards);
    let bind_interval = cfg.bind_retry_interval;
    let bind_deadline = cfg.bind_retry_deadline;
    let is_udp_in_use = |e: &BindError| e.is_addr_in_use();
    let is_tcp_in_use = |e: &std::io::Error| e.kind() == std::io::ErrorKind::AddrInUse;

    let metrics =
        bind_with_retry("metrics", metrics_sa, bind_interval, bind_deadline, is_tcp_in_use, || {
            tokio::net::TcpListener::bind(metrics_sa)
        })
        .await?;
    // The probe server reads the address when it starts: a listener that
    // cannot name it refuses the boot here, before any SIP socket is bound.
    metrics.local_addr().map_err(|e| format!("metrics listener on {metrics_sa}: {e}"))?;

    // Main signaling endpoint(s) — one per recv shard. With N > 1 every bind
    // (including the first) sets SO_REUSEPORT; the kernel flow-hashes on the
    // 4-tuple so all datagrams from one UAC src:port land on ONE socket and
    // per-flow ordering (INVITE→CANCEL, retransmits) is preserved.
    //
    // Every signaling bind (both faces, all shards) carries the selective
    // intake-shed hook — a blind tail-drop at the queue cap would otherwise
    // kill emergency and in-dialog traffic first-come-first-served.
    let watermark = intake_shed_watermark(queue_max, cfg.intake_shed_pct);
    let intake_shed = intake_shed_hook(watermark, proxy_metrics.clone());
    let with_buffers = |mut opts: BindUdpOpts| {
        if let Some(bytes) = udp_sndbuf {
            opts = opts.with_send_buffer(bytes);
        }
        if let Some(bytes) = udp_rcvbuf {
            opts = opts.with_recv_buffer(bytes);
        }
        opts
    };
    let mut internal = Vec::with_capacity(recv_shards);
    for _ in 0..recv_shards {
        let ep = bind_with_retry(
            "internal face",
            listen_sa,
            bind_interval,
            bind_deadline,
            is_udp_in_use,
            || {
                let opts = with_buffers(
                    BindUdpOpts::new(listen_sa, queue_max)
                        .with_reuse_port(recv_shards > 1)
                        .with_pre_ingress(intake_shed.clone())
                        .with_clock(clock.clone()),
                );
                async move { net.bind_udp(opts).await }
            },
        )
        .await?;
        internal.push(ep);
    }

    // External-face sockets, one per recv shard — the SO_REUSEPORT recv
    // sharding applies to BOTH faces identically (kernel flow-hashing keeps
    // each caller src:port on one socket per face, so per-flow ordering holds
    // on the external plane too).
    let mut external = Vec::new();
    if let Some(ExternalFace { listen: ext_sa, .. }) = &cfg.external {
        for _ in 0..recv_shards {
            let ep = bind_with_retry(
                "external face",
                *ext_sa,
                bind_interval,
                bind_deadline,
                is_udp_in_use,
                || {
                    let opts = with_buffers(
                        BindUdpOpts::new(*ext_sa, queue_max)
                            .with_reuse_port(recv_shards > 1)
                            .with_pre_ingress(intake_shed.clone())
                            .with_clock(clock.clone()),
                    );
                    async move { net.bind_udp(opts).await }
                },
            )
            .await?;
            external.push(ep);
        }
    }
    // Every shard of a face requests the same buffers: its first socket speaks
    // for the face.
    if let Some(ep) = internal.first() {
        log_socket_buffers("internal", ep.as_ref(), udp_rcvbuf, udp_sndbuf);
    }
    if let Some(ep) = external.first() {
        log_socket_buffers("external", ep.as_ref(), udp_rcvbuf, udp_sndbuf);
    }

    // Separate endpoint for the OPTIONS health probe (its own source socket).
    // Port 0 (ephemeral) never conflicts; the retry wrapper keeps the fatal
    // contract uniform across every bind.
    let probe_sa = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let probe = bind_with_retry(
        "probe socket",
        probe_sa,
        bind_interval,
        bind_deadline,
        is_udp_in_use,
        || {
            let opts = with_buffers(BindUdpOpts::new(probe_sa, 1024).with_clock(clock.clone()));
            async move { net.bind_udp(opts).await }
        },
    )
    .await?;

    let mut external = external.into_iter();
    let shards =
        internal.into_iter().map(|internal| ShardSockets { internal, external: external.next() });
    Ok(Sockets { metrics, shards: shards.collect(), probe })
}

/// Log the buffer sizes the kernel granted one face's signalling socket,
/// warning when it clamped a request ([`was_clamped`]).
fn log_socket_buffers(
    face: &str,
    endpoint: &dyn UdpEndpoint,
    recv_requested: Option<usize>,
    send_requested: Option<usize>,
) {
    let Some(granted) = endpoint.socket_buffers() else { return };
    tracing::info!(
        face,
        rcvbuf = granted.recv,
        rcvbuf_requested = ?recv_requested,
        sndbuf = granted.send,
        sndbuf_requested = ?send_requested,
        "signalling socket buffers"
    );
    for (knob, requested, effective, cap) in [
        ("PROXY_UDP_RCVBUF", recv_requested, granted.recv, "net.core.rmem_max"),
        ("PROXY_UDP_SNDBUF", send_requested, granted.send, "net.core.wmem_max"),
    ] {
        if let Some(requested) = requested.filter(|&r| was_clamped(r, effective)) {
            tracing::warn!(
                face,
                knob,
                requested,
                effective,
                "the kernel clamped the socket buffer: raise {cap}"
            );
        }
    }
}

/// Bounded bind retry for a port a predecessor process may still hold.
///
/// A recreated `hostNetwork` pod lands in the node netns its predecessor just
/// vacated, so a bind can hit a transient `EADDRINUSE` while the old sockets
/// linger. Re-attempts `bind` every `interval` while `is_addr_in_use(&err)`,
/// logging each wait so a slow release is visible, until `deadline`. The
/// default deadline (10 s) deliberately exceeds the 5 s SIGTERM drain grace so
/// a fast pod recreation waits out its predecessor — instances serialize on
/// the node, never overlap.
///
/// Any other bind error, or addr-in-use past the deadline, returns a message
/// naming the address. The caller MUST treat that as fatal (a refused boot,
/// non-zero exit): the process never runs with a subset of its ports.
pub async fn bind_with_retry<T, E, Fut>(
    what: &str,
    addr: SocketAddr,
    interval: Duration,
    deadline: Duration,
    is_addr_in_use: impl Fn(&E) -> bool,
    mut bind: impl FnMut() -> Fut,
) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let started = tokio::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match bind().await {
            Ok(t) => return Ok(t),
            Err(e) if !is_addr_in_use(&e) => {
                return Err(format!("bind {what} {addr} failed: {e}"));
            }
            Err(e) => {
                if started.elapsed() + interval > deadline {
                    return Err(format!(
                        "bind {what} {addr} still in use after {attempt} attempt(s) over {}ms — \
                         the address belongs to someone else: {e}",
                        started.elapsed().as_millis(),
                    ));
                }
                tracing::warn!(%addr, bind = what, attempt, "waiting for address release");
                tokio::time::sleep(interval).await;
            }
        }
    }
}

#[cfg(test)]
mod tests;
