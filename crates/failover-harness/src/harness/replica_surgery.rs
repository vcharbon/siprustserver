//! HARNESS SURGERY on a worker's replicas: a body written straight into its
//! store, as a pull that lost versions or lagged leaves it. Each marks the
//! run tampered.

use std::sync::atomic::Ordering;

use b2bua::store::{CallStore, PartitionRole, PutOpts};

use super::ReplicatedB2buaSut;

impl ReplicatedB2buaSut {
    /// Rewind the replica body this node holds for `call_ref` in `(role,
    /// primary)` to `body` — an earlier body it genuinely held (read back with
    /// [`get`](Self::get)) — as if every version after it died with the
    /// crashed primary before the peer pulled it. The stored `(p,b)` is kept,
    /// so bootstrap and reclaim replay the rewound body exactly as they would
    /// the latest one.
    pub async fn rewind_replica(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
    ) {
        self.harness.tampered.store(true, Ordering::Relaxed);
        let (p, b) =
            self.store.current_cv(role, primary, call_ref).expect("replica version vector present");
        self.store
            .put_call(role, primary, call_ref, body, &[], 600_000, p, b, &PutOpts::default())
            .await
            .expect("replica store write");
    }

    /// Land in this node's `bak:{primary}` the `body` its primary wrote for
    /// `call_ref` at version `(p, b)`, both read back from the primary
    /// ([`get`](Self::get), [`call_gen`](Self::call_gen),
    /// [`call_bgen`](Self::call_bgen)), as a pull that lagged behind the
    /// primary's death lands it once the peer departed.
    pub async fn land_replica(
        &self,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
        (p, b): (i64, i64),
    ) {
        self.harness.tampered.store(true, Ordering::Relaxed);
        let role = PartitionRole::Backup;
        self.store
            .put_call(role, primary, call_ref, body, &[], 600_000, p, b, &PutOpts::default())
            .await
            .expect("replica store write");
    }
}
