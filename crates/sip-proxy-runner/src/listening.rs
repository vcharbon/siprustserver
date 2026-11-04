//! The proxy's lifecycle line once it is about to serve.

use sip_proxy::registry::WorkerRegistry;

use crate::config::ProxyConfig;

/// The lifecycle line of a proxy about to serve: its faces, its workers and
/// its socket settings.
pub fn log_listening(cfg: &ProxyConfig, registry: &dyn WorkerRegistry) {
    let face_note = match &cfg.external {
        Some(ext) => format!(
            " DUAL-FACE ext_listen={} ext_advertise={}:{} (int CIDRs from PROXY_FACE_INT_CIDRS)",
            ext.listen, ext.advertise.host, ext.advertise.port,
        ),
        None => String::new(),
    };
    tracing::info!(
        pid = std::process::id(),
        listen = %cfg.listen,
        advertise = %format_args!("{}:{}", cfg.advertise.host, cfg.advertise.port),
        faces = %face_note,
        workers = %registry
            .snapshot()
            .iter()
            .map(|w| format!("{}@{}:{}", w.id, w.address.host, w.address.port))
            .collect::<Vec<_>>()
            .join(","),
        queue = cfg.queue_max,
        recv_shards = cfg.recv_shards,
        udp_sndbuf = ?cfg.udp_sndbuf,
        udp_rcvbuf = ?cfg.udp_rcvbuf,
        shard_stall_ms = cfg.shard_stall_ms,
        "listening"
    );
}
