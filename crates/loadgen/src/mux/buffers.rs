//! The kernel socket buffers each mux endpoint requests. One endpoint carries
//! every concurrent call's datagrams, so a burst the dispatcher has not drained
//! yet sits in its `SO_RCVBUF`; what overflows it the kernel drops before the
//! loadgen sees it, and the call it belonged to fails on a timeout that is no
//! SUT behaviour. A large request keeps the loadgen's own socket from being the
//! loss point. The simulated fabric ignores the request.

use sip_net::socket_stats::was_clamped;
use sip_net::{BindUdpOpts, UdpEndpoint};

/// `SO_RCVBUF` and `SO_SNDBUF` every mux endpoint requests, in bytes. The
/// kernel clamps the request at `net.core.rmem_max` / `wmem_max`.
const MUX_SOCKET_BUFFER: usize = 4 << 20;

/// `opts` requesting the mux socket buffers.
pub(super) fn with_mux_buffers(opts: BindUdpOpts) -> BindUdpOpts {
    opts.with_recv_buffer(MUX_SOCKET_BUFFER).with_send_buffer(MUX_SOCKET_BUFFER)
}

/// Warn on stderr when the kernel granted `endpoint` less than the request.
/// Silent for a fabric with no kernel socket.
pub(super) fn warn_if_clamped(endpoint: &dyn UdpEndpoint) {
    let Some(granted) = endpoint.socket_buffers() else { return };
    for (what, effective, cap) in [
        ("rcvbuf", granted.recv, "net.core.rmem_max"),
        ("sndbuf", granted.send, "net.core.wmem_max"),
    ] {
        if was_clamped(MUX_SOCKET_BUFFER, effective) {
            eprintln!(
                "[loadgen] {}: the kernel clamped the {what} request {MUX_SOCKET_BUFFER} to {effective}: raise {cap}",
                endpoint.local_addr()
            );
        }
    }
}
