//! Buffered terminate-path writer — port of `BufferedTerminateWriter`. Decouples
//! the router from the store: `submit_*` never blocks (drop-on-full), a drainer
//! task performs the actual `put`/`delete`. The in-memory store can't stall, but
//! keeping the seam identical means the future replicating store slots in
//! unchanged.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::call_store::{CallStore, PartitionRole, PutOpts};

enum TerminateOp {
    Put {
        role: PartitionRole,
        primary: String,
        call_ref: String,
        body: Vec<u8>,
        indexes: Vec<String>,
        ttl_ms: i64,
        call_gen: i64,
        opts: PutOpts,
    },
    Delete {
        role: PartitionRole,
        primary: String,
        call_ref: String,
        indexes: Vec<String>,
        opts: PutOpts,
    },
}

/// Non-blocking submit handle. Clone-cheap.
#[derive(Clone)]
pub struct BufferedTerminateWriter {
    tx: mpsc::Sender<TerminateOp>,
}

impl BufferedTerminateWriter {
    /// Spawn the drainer over `store`. The task lives until all writer clones
    /// drop.
    pub fn spawn(store: Arc<dyn CallStore>, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        tokio::spawn(drain(store, rx));
        Self { tx }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_put(
        &self,
        role: PartitionRole,
        primary: String,
        call_ref: String,
        body: Vec<u8>,
        indexes: Vec<String>,
        ttl_ms: i64,
        call_gen: i64,
        opts: PutOpts,
    ) {
        let _ = self.tx.try_send(TerminateOp::Put {
            role,
            primary,
            call_ref,
            body,
            indexes,
            ttl_ms,
            call_gen,
            opts,
        });
    }

    pub fn submit_delete(
        &self,
        role: PartitionRole,
        primary: String,
        call_ref: String,
        indexes: Vec<String>,
        opts: PutOpts,
    ) {
        let _ = self.tx.try_send(TerminateOp::Delete {
            role,
            primary,
            call_ref,
            indexes,
            opts,
        });
    }
}

async fn drain(store: Arc<dyn CallStore>, mut rx: mpsc::Receiver<TerminateOp>) {
    while let Some(op) = rx.recv().await {
        match op {
            TerminateOp::Put {
                role,
                primary,
                call_ref,
                body,
                indexes,
                ttl_ms,
                call_gen,
                opts,
            } => {
                let _ = store
                    .put_call(role, &primary, &call_ref, body, &indexes, ttl_ms, call_gen, &opts)
                    .await;
            }
            TerminateOp::Delete {
                role,
                primary,
                call_ref,
                indexes,
                opts,
            } => {
                let _ = store
                    .delete_call(role, &primary, &call_ref, &indexes, &opts)
                    .await;
            }
        }
    }
}
