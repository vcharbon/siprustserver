//! Building a puller: the peer and flow it serves, how it resolves the peer's
//! address, and the watermark it resumes from.

use std::net::SocketAddr;
use std::sync::Arc;

use call::MsgpackCodec;
use repl_net::frame::{Partition, Watermark};
use repl_net::transport::ReplicationNetwork;
use tokio::sync::watch;
use topology::Peer;

use super::{Puller, PullerConfig, PullerStatus};
use crate::repl::{AddrResolver, ReplicatingCallStore};

impl Puller {
    /// Build a puller for `peer` on `partition` (the flow), resolving its address
    /// per-connect via `resolve` (ADR-0012 D3), seeded from `start_w` (the
    /// supervisor's retained watermark — `(0,0)` for a cold/never-seen peer).
    /// Returns the puller plus a [`watch::Receiver`] the supervisor reads.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer: Peer,
        self_ordinal: impl Into<String>,
        partition: Partition,
        resolve: AddrResolver,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        config: PullerConfig,
        start_w: Watermark,
        metrics: crate::metrics::B2buaMetrics,
    ) -> (Self, watch::Receiver<PullerStatus>) {
        // A warm resume (W > (0,0)) needs no bootstrap — mark it complete up
        // front so readiness doesn't wait on a bootstrap that won't happen.
        let warm = start_w != Watermark::new(0, 0);
        let (status_tx, status_rx) = watch::channel(PullerStatus {
            current: false,
            watermark: start_w,
            bootstrap_complete: warm,
            reset_gen: 0,
            ever_connected: false,
        });
        (
            Self {
                peer,
                self_ordinal: self_ordinal.into(),
                identity: None,
                partition,
                resolve,
                network,
                store,
                config,
                status_tx,
                metrics,
                repl_tx: None,
                capacity: None,
                turn_probe: Default::default(),
                codec: MsgpackCodec::new(),
                bootstrap_started_at: std::sync::Mutex::new(
                    (!warm).then(tokio::time::Instant::now),
                ),
                held_meetings: Default::default(),
            },
            status_rx,
        )
    }

    /// Convenience constructor for a puller that always connects to a **fixed**
    /// `peer_addr` (a resolver-less caller / tests with a concrete bound socket).
    /// Wraps `peer_addr` in a constant [`FnPeerResolver`](super::FnPeerResolver).
    #[allow(clippy::too_many_arguments)]
    pub fn new_at(
        peer_ordinal: impl Into<String>,
        self_ordinal: impl Into<String>,
        partition: Partition,
        peer_addr: SocketAddr,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        config: PullerConfig,
        start_w: Watermark,
        metrics: crate::metrics::B2buaMetrics,
    ) -> (Self, watch::Receiver<PullerStatus>) {
        let ordinal = peer_ordinal.into();
        let resolve: AddrResolver =
            Arc::new(crate::repl::FnPeerResolver(move |_: &Peer| peer_addr));
        Self::new(
            Peer::new(ordinal, peer_addr.to_string()),
            self_ordinal,
            partition,
            resolve,
            network,
            store,
            config,
            start_w,
            metrics,
        )
    }
}
