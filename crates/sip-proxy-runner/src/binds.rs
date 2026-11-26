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
use sip_proxy::intake_shed::{intake_shed_hook, intake_shed_watermark, IntakeFace};
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::worker_signs::WorkerSigns;
use sip_proxy::ProxyMetrics;

use crate::config::{ExternalFace, ProxyConfig};

pub struct Sockets {
    /// The metrics/probe HTTP listener, its address read; its accept loop
    /// starts later.
    pub metrics: tokio::net::TcpListener,
    pub shards: Vec<ShardSockets>,
    pub probe: Box<dyn UdpEndpoint>,
    /// The dead notifier's own source socket (ADR-0050 §3).
    pub notice: Box<dyn UdpEndpoint>,
}

pub struct ShardSockets {
    pub internal: Box<dyn UdpEndpoint>,
    /// Its external-face endpoint: one exactly when the proxy has an
    /// external face.
    pub external: Option<Box<dyn UdpEndpoint>>,
}

/// Bind every socket `cfg` names, each signalling endpoint stamping arrivals
/// on `clock`, in this order: the metrics/probe HTTP listener, the
/// internal-face signalling sockets, the external-face signalling sockets, the
/// probe and notice sockets. The metrics listener is the node-singleton lock;
/// its accept loop starts later.
///
/// `Err` names the first socket that did not bind: the boot stops on it.
/// `proxy_metrics` counts what the signalling sockets' intake hook drops.
/// The internal face's hook reads a worker's signs over `registry`, the pool
/// the cores route over, so a worker it lists later is read as one from then
/// on; the external face's trusts none (`IntakeFace`).
pub async fn bind_sockets(
    cfg: &ProxyConfig,
    clock: &Clock,
    proxy_metrics: &Arc<ProxyMetrics>,
    registry: &Arc<dyn WorkerRegistry>,
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
    metrics.local_addr().map_err(|e| format!("metrics listener on {metrics_sa}: {e}"))?;

    let watermark = intake_shed_watermark(queue_max, cfg.intake_shed_pct);
    let external_advertise = cfg.external.as_ref().map(|ext| ext.advertise.clone());
    let signs = WorkerSigns::new(registry.clone(), cfg.advertise.clone(), external_advertise);
    let shed_on = |face| intake_shed_hook(watermark, proxy_metrics.clone(), cfg.parser(), face);
    let (internal_shed, external_shed) =
        (shed_on(IntakeFace::Internal(signs)), shed_on(IntakeFace::External));
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
                        .with_pre_ingress(internal_shed.clone())
                        .with_clock(clock.clone()),
                );
                async move { net.bind_udp(opts).await }
            },
        )
        .await?;
        internal.push(ep);
    }

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
                            .with_pre_ingress(external_shed.clone())
                            .with_clock(clock.clone()),
                    );
                    async move { net.bind_udp(opts).await }
                },
            )
            .await?;
            external.push(ep);
        }
    }
    if let Some(ep) = internal.first() {
        log_socket_buffers("internal", ep.as_ref(), udp_rcvbuf, udp_sndbuf);
    }
    if let Some(ep) = external.first() {
        log_socket_buffers("external", ep.as_ref(), udp_rcvbuf, udp_sndbuf);
    }

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

    let notice = bind_with_retry(
        "notice socket",
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
    Ok(Sockets { metrics, shards: shards.collect(), probe, notice })
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
/// Re-attempts `bind` every `interval` while `is_addr_in_use(&err)`, logging
/// each wait, until `deadline`. The configured deadline exceeds the SIGTERM
/// drain grace (the configuration refuses otherwise), so a recreated
/// `hostNetwork` pod waits out its predecessor: instances on one node
/// serialize, never overlap.
///
/// Any other bind error, or addr-in-use past the deadline, returns a message
/// naming the address. The caller treats it as fatal: the process never runs
/// with a subset of its ports.
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
