//! The kernel socket buffers each mux endpoint requests. One endpoint carries
//! every concurrent call's datagrams, so a burst the dispatcher has not drained
//! yet sits in its `SO_RCVBUF`; what overflows it the kernel drops before the
//! loadgen sees it, and the call it belonged to fails on a timeout that is no
//! SUT behaviour. A large request keeps the loadgen's own socket from being the
//! loss point.

use sip_net::socket_stats::was_clamped;
use sip_net::{BindUdpOpts, UdpEndpoint};

/// `SO_RCVBUF` / `SO_SNDBUF` to request on every mux endpoint, in bytes. `None`
/// keeps the kernel default (`rmem_default` / `wmem_default`); the kernel
/// clamps a request at `rmem_max` / `wmem_max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketBufferRequest {
    pub recv: Option<usize>,
    pub send: Option<usize>,
}

impl SocketBufferRequest {
    /// Leave both buffers at the kernel default.
    pub const KERNEL_DEFAULT: Self = SocketBufferRequest { recv: None, send: None };
    /// The load generator's default request: 4 MiB each way.
    pub const LOADGEN_DEFAULT: Self =
        SocketBufferRequest { recv: Some(4 << 20), send: Some(4 << 20) };

    /// `opts` with these buffers requested.
    pub(crate) fn apply(self, mut opts: BindUdpOpts) -> BindUdpOpts {
        if let Some(bytes) = self.recv {
            opts = opts.with_recv_buffer(bytes);
        }
        if let Some(bytes) = self.send {
            opts = opts.with_send_buffer(bytes);
        }
        opts
    }

    /// Report the buffers the kernel granted `endpoint` on stderr, and warn when
    /// it clamped a request. Silent for a fabric with no kernel socket.
    pub(crate) fn report(self, endpoint: &dyn UdpEndpoint) {
        let Some(granted) = endpoint.socket_buffers() else { return };
        eprintln!(
            "[loadgen] {} socket buffers: rcvbuf {} (requested {:?}), sndbuf {} (requested {:?})",
            endpoint.local_addr(),
            granted.recv,
            self.recv,
            granted.send,
            self.send
        );
        for (flag, requested, effective, cap) in [
            ("--udp-rcvbuf", self.recv, granted.recv, "net.core.rmem_max"),
            ("--udp-sndbuf", self.send, granted.send, "net.core.wmem_max"),
        ] {
            if let Some(requested) = requested.filter(|&r| was_clamped(r, effective)) {
                eprintln!(
                    "[loadgen] the kernel clamped {flag} {requested} to {effective}: raise {cap}"
                );
            }
        }
    }
}
