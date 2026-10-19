//! The supervised membership reconcile (ADR-0012 D1/D2): the boot pass runs
//! synchronously under a subscription taken before it; every later run
//! follows that subscription, or, restarted after a panic, subscribes and
//! reconciles inside the run before it follows.

use std::sync::Arc;

use tokio::task::JoinHandle;
use topology::Membership;

use super::ReplicationSupervisor;
use crate::supervise::{spawn, MEMBERSHIP_RECONCILE};

impl ReplicationSupervisor {
    /// Reconcile from `membership`'s snapshot now, then spawn the follower
    /// of its deltas and periodic ticks.
    pub(super) fn spawn_membership_follow(
        &self,
        membership: Arc<dyn Membership>,
    ) -> JoinHandle<()> {
        let period = self.inner.reconcile_period;
        let mut boot_changes = Some(membership.changes());
        self.reconcile_from_snapshot(membership.snapshot());
        let this = self.clone();
        spawn(MEMBERSHIP_RECONCILE, move || {
            let (this, membership, boot) = (this.clone(), membership.clone(), boot_changes.take());
            async move {
                let changes = match boot {
                    Some(changes) => changes,
                    None => {
                        let changes = membership.changes();
                        this.reconcile_from_snapshot(membership.snapshot());
                        changes
                    }
                };
                topology::follow_membership(membership, changes, period, move |snapshot| {
                    this.reconcile_from_snapshot(snapshot);
                })
                .await;
            }
        })
    }
}
