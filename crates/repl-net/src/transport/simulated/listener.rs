//! [`SimListener`] — the accepting side of a simulated listen address.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::SimShared;
use crate::transport::{AcceptError, ReplicationConnection, ReplicationListener};

/// A bound simulated listener: the server ends of the connects made to its
/// address, in order. Dropping it frees the address.
pub(super) struct SimListener {
    local: SocketAddr,
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<Box<dyn ReplicationConnection>>>,
    shared: Arc<SimShared>,
}

impl SimListener {
    pub(super) fn new(
        local: SocketAddr,
        incoming: mpsc::UnboundedReceiver<Box<dyn ReplicationConnection>>,
        shared: Arc<SimShared>,
    ) -> Self {
        Self { local, incoming: tokio::sync::Mutex::new(incoming), shared }
    }

    /// Spend one armed accept failure, if any is left.
    fn take_accept_error(&self) -> bool {
        let mut errors = self.shared.accept_errors.lock().unwrap();
        match errors.get_mut(&self.local) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        }
    }
}

#[async_trait]
impl ReplicationListener for SimListener {
    async fn accept(&self) -> Result<Box<dyn ReplicationConnection>, AcceptError> {
        if self.take_accept_error() {
            return Err(AcceptError::Io("simulated accept error".into()));
        }
        self.incoming.lock().await.recv().await.ok_or(AcceptError::Closed)
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for SimListener {
    fn drop(&mut self) {
        self.shared.routing.lock().unwrap().remove(&self.local);
    }
}
