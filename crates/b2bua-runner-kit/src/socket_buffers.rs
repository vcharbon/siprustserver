//! What the kernel granted the signalling socket's buffers, logged against
//! what the `B2BUA_UDP_*BUF` knobs requested.

use sip_net::socket_stats::was_clamped;
use sip_net::UdpEndpoint;

/// Log the buffer sizes the kernel granted the signalling socket, warning when
/// it clamped a request ([`was_clamped`]).
pub(crate) fn log_socket_buffers(
    service: &str,
    endpoint: &dyn UdpEndpoint,
    recv_requested: Option<usize>,
    send_requested: Option<usize>,
) {
    let Some(granted) = endpoint.socket_buffers() else { return };
    tracing::info!(
        service,
        rcvbuf = granted.recv,
        rcvbuf_requested = ?recv_requested,
        sndbuf = granted.send,
        sndbuf_requested = ?send_requested,
        "signalling socket buffers"
    );
    for (knob, requested, effective, cap) in [
        ("B2BUA_UDP_RCVBUF", recv_requested, granted.recv, "net.core.rmem_max"),
        ("B2BUA_UDP_SNDBUF", send_requested, granted.send, "net.core.wmem_max"),
    ] {
        if let Some(requested) = requested.filter(|&r| was_clamped(r, effective)) {
            tracing::warn!(
                service,
                knob,
                requested,
                effective,
                "the kernel clamped the socket buffer: raise {cap}"
            );
        }
    }
}
