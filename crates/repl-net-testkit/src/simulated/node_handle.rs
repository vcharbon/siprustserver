//! [`NodeReplicationNetwork`] — one node's own handle on the simulated fabric.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(doc)]
use super::Fault;
use super::{synth_local, SimShared, SimulatedReplicationNetwork};
use crate::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
};

/// One node's handle on a [`SimulatedReplicationNetwork`]: `connect` draws the
/// ephemeral local and attributes it to the node's listen address before the
/// fabric decides, so a [`Fault::Partition`] on the node refuses the
/// connect with [`ConnectError::Blocked`] — a peer behind a partition is never
/// reached — and a node [`Fault::Cut`] closes the stream at once. `listen` is
/// the fabric's own.
#[derive(Clone)]
pub struct NodeReplicationNetwork {
    shared: Arc<SimShared>,
    /// The listen address of the node the handle belongs to.
    owner: SocketAddr,
}

impl NodeReplicationNetwork {
    /// The handle of the node listening at `owner` on the fabric `shared`.
    pub(super) fn new(shared: Arc<SimShared>, owner: SocketAddr) -> Self {
        Self { shared, owner }
    }
}

#[async_trait]
impl ReplicationNetwork for NodeReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let local = synth_local(&self.shared, dst);
        self.shared.stream_owner.lock().unwrap().insert(local, self.owner);
        let fabric = SimulatedReplicationNetwork { shared: self.shared.clone() };
        let conn = fabric.connect_from(local, dst).await;
        if conn.is_err() {
            self.shared.forget_stream(local);
        }
        conn
    }

    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
        SimulatedReplicationNetwork { shared: self.shared.clone() }.listen(local).await
    }
}
