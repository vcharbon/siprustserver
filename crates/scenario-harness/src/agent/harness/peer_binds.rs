//! Raw endpoints for simulated parties: byte-level fixture peers bound on the
//! shared, recording-wrapped fabric, their findings the party's own.

use std::collections::HashSet;
use std::net::SocketAddr;

use layer_harness::NetworkTag;
use sip_net::{BindUdpOpts, PreIngressHook, UdpEndpoint};

use super::{Harness, SUT_QUEUE_MAX};

impl Harness {
    /// Bind a raw endpoint for a SIMULATED party (a byte-level fixture peer) on
    /// the shared, recording-wrapped fabric: a UA-role (`{Uac, Uas}`) bind on
    /// an `Ext` lane, so its findings are the party's own, never the SUT's
    /// (a party-less waiver covers them). The SUT itself binds with
    /// [`bind_sut`](Self::bind_sut).
    pub async fn bind_peer(
        &self,
        name: impl Into<String>,
        addr: &str,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        self.bind_peer_opts(name, addr, None).await
    }

    /// [`bind_peer`](Self::bind_peer) with an arrival-time `hook`, run on every
    /// datagram before it is queued: a peer whose inbound path loses what the
    /// hook drops.
    pub async fn bind_peer_with_pre_ingress(
        &self,
        name: impl Into<String>,
        addr: &str,
        hook: PreIngressHook,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        self.bind_peer_opts(name, addr, Some(hook)).await
    }

    async fn bind_peer_opts(
        &self,
        name: impl Into<String>,
        addr: &str,
        pre_ingress: Option<PreIngressHook>,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.addrs.note_bound(addr);
        self.recorder.register_lane(addr, name.into(), NetworkTag::Ext);
        let roles = HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]);
        let mut opts = BindUdpOpts::new(addr, SUT_QUEUE_MAX).with_roles(roles);
        if let Some(hook) = pre_ingress {
            opts = opts.with_pre_ingress(hook);
        }
        let ep =
            self.network.bind_udp(opts).await.unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }
}
