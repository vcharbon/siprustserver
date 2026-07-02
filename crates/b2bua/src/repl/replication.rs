//! `replication` — the **write-side policy** (migration slice S8). Given a node
//! ordinal and a `callRef`, it answers the two questions a mutation must resolve
//! before it touches the [`CallStore`](crate::store::CallStore):
//!
//! 1. **Which partition does the body land in?** — `pri:{primary}` if I am the
//!    call's natural primary, `bak:{primary}` if I am acting-backup for a crashed
//!    peer (resolved by [`partition_of`](crate::store::partition_of)).
//! 2. **Whom do I propagate to, and in which direction?** —
//!    [`replication_target`]: a primary pushes **Forward** to its backup; an
//!    acting-backup pushes **Reverse** to the original primary so the primary
//!    RECLAIMS the latest state on reboot.
//!
//! The apply side (S5/S6) already maps the direction back: `Reverse` rides the
//! frame as `partition=Pri` (see [`ReplicatingCallStore`] / [`Changelog`]) and
//! the puller imports a `partition=Pri` frame as `pri:{primary}` — so a reverse
//! write by the acting-backup arrives on the rebooting primary as its own
//! `pri:` partition. This module closes the write half of that round-trip.
//!
//! [`ReplicatingCallStore`]: super::store::ReplicatingCallStore
//! [`Changelog`]: super::changelog::Changelog
//!
//! ## `(p,b)` version-vector bump rule (ADR-0014)
//! **Each authoritative mutation increments the *local node's own* counter** of the
//! call's `(p, b)` = `CallTopology.{gen, bak_gen}`: a primary bumps `p`, an
//! acting-backup bumps `b`. Because each node touches only its own counter, the
//! other counter on a propagated update is the **branch point**, which lets the
//! direction-aware merge (`apply_to_store`) resolve concurrent primary+backup
//! mutations: Forward/Bootstrap defer to the authority unless dominated; Reverse
//! applies iff `p_in == p_cur && b_in > b_cur`; deletes win both ways. (Was a
//! single `call_gen` LWW — see ADR-0014 §5.2 for the divergence that closed.)
//!
//! The actual `topology.gen += 1` increment is wired in the b2bua dispatch /
//! `CallState` mutation path — **S10 wiring point**. At this layer the tests
//! drive `call_gen` explicitly on the encoded body; this module only routes the
//! already-stamped gen to the right partition/peer/direction.

use crate::store::{partition_of, CallStore, PartitionRole, PropagateDirection, PutOpts, StoreError};
use call::parse_call_ref;

/// Resolve a `callRef`'s primary ordinal, mirroring [`partition_of`]'s ownership
/// rule. A legacy ref with no encoded ordinal is treated as owned by `self`.
fn primary_of(self_ordinal: &str, call_ref: &str) -> String {
    match parse_call_ref(call_ref) {
        Some(p) => p.primary,
        None => self_ordinal.to_string(),
    }
}

/// Decide where a mutation of `call_ref` replicates, and in which direction.
///
/// - I am the call's **primary** (the ref's encoded ordinal is mine) →
///   `Some((backup_resolver(call_ref)?, Forward))`: push the body to my backup.
/// - I am **acting-backup** (the ref names a *different* primary — that node
///   crashed and the proxy failed the dialog over to me) →
///   `Some((call_ref.primary, Reverse))`: push the body BACK to the original
///   primary so it reclaims on reboot.
/// - `None` when no backup peer is resolvable on the primary path (single-node /
///   no peers): store locally, propagate nothing.
///
/// `backup_resolver` is **injected**, not computed here.
///
/// ### S10 seam — sourcing the backup peer (do NOT build HRW in the b2bua)
/// S10 sources the backup peer: the proxy already computed it as the rendezvous
/// (HRW) **2nd-best** keyed by Call-ID and signed it into the `w_bak` stickiness
/// cookie; the b2bua will read `w_bak` from the cookie so the two AGREE by
/// construction rather than risk a divergent recompute (the proxy keys HRW off
/// the *alive-set* + Call-ID, which the b2bua cannot reproduce locally). See the
/// sip-proxy `rendezvous_select` / `load_balancer.rs`. Until then this resolver
/// is injected (tests pass a trivial 2-node "the other node" closure).
pub fn replication_target(
    self_ordinal: &str,
    call_ref: &str,
    backup_resolver: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, PropagateDirection)> {
    let primary = primary_of(self_ordinal, call_ref);
    if primary == self_ordinal {
        // I'm PRIMARY → forward to my backup (if one is resolvable).
        backup_resolver(call_ref).map(|backup| (backup, PropagateDirection::Forward))
    } else {
        // I'm ACTING-BACKUP for a crashed peer → reverse-propagate to it.
        Some((primary, PropagateDirection::Reverse))
    }
}

/// The STORE `(role, primary)` a mutation lands in, paired with its replication
/// target. Ties [`partition_of`] (where the body lives) to [`replication_target`]
/// (where it propagates) so the two never drift.
///
/// - primary path → store `(Primary, self)`, target `(backup, Forward)`.
/// - acting-backup path → store `(Backup, call_ref.primary)`, target
///   `(primary, Reverse)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicationPlan {
    pub role: PartitionRole,
    pub primary: String,
    /// `None` on the single-node primary path (store locally, no propagation).
    pub target: Option<(String, PropagateDirection)>,
}

impl ReplicationPlan {
    /// Compute the full store-and-propagate plan for `call_ref`.
    pub fn resolve(
        self_ordinal: &str,
        call_ref: &str,
        backup_resolver: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let (role, primary) = partition_of(self_ordinal, call_ref);
        let target = replication_target(self_ordinal, call_ref, backup_resolver);
        Self { role, primary, target }
    }

    /// Build the [`PutOpts`] this plan implies (the propagate peer + direction,
    /// or an empty `PutOpts` for the local-only single-node path).
    pub fn put_opts(&self) -> PutOpts {
        match &self.target {
            Some((peer, direction)) => PutOpts {
                peer: Some(peer.clone()),
                direction: Some(*direction),
                // A local flush of THIS node's own call — its timer deadlines are
                // already on this node's clock, so no receive-time skew to record.
                ..PutOpts::default()
            },
            None => PutOpts::default(),
        }
    }
}

/// Flush a call's already-encoded body to `store` under the S8 write-side policy:
/// resolve the partition + propagate target for `call_ref`, then `put_call` with
/// the matching `(role, primary)` and [`PutOpts`].
///
/// This is the reusable seam **S10 calls from `CallState::flush`** (today that
/// flush still uses `PutOpts::default()` — no propagation; S10 swaps it for this).
/// The caller supplies the body / indexes / ttl / `call_gen` (the primary counter
/// `p` of the `(p,b)` version vector, = `CallTopology.gen`); this routes them.
#[allow(clippy::too_many_arguments)]
pub async fn flush_replicated(
    store: &dyn CallStore,
    self_ordinal: &str,
    call_ref: &str,
    body: Vec<u8>,
    indexes: &[String],
    ttl_ms: i64,
    call_gen: i64,
    call_bgen: i64,
    backup_resolver: &dyn Fn(&str) -> Option<String>,
) -> Result<ReplicationPlan, StoreError> {
    let plan = ReplicationPlan::resolve(self_ordinal, call_ref, backup_resolver);
    let opts = plan.put_opts();
    store
        .put_call(
            plan.role,
            &plan.primary,
            call_ref,
            body,
            indexes,
            ttl_ms,
            call_gen,
            call_bgen,
            &opts,
        )
        .await?;
    Ok(plan)
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn cref(primary: &str, id: &str) -> String {
        format!("{primary}|{id}|t{id}")
    }

    /// The trivial 2-node resolver tests inject: the backup is "the other node".
    fn other(self_ordinal: &'static str, peer: &'static str) -> impl Fn(&str) -> Option<String> {
        move |_call_ref: &str| {
            let _ = self_ordinal;
            Some(peer.to_string())
        }
    }

    #[test]
    fn primary_self_targets_backup_forward() {
        // A owns "A|..|.." → forward to its backup B.
        let resolver = other("A", "B");
        let got = replication_target("A", &cref("A", "1"), &resolver);
        assert_eq!(got, Some(("B".to_string(), PropagateDirection::Forward)));
    }

    #[test]
    fn acting_backup_targets_primary_reverse() {
        // B handling "A|..|.." (A crashed) → reverse-propagate back to A. The
        // resolver is irrelevant on the reverse path (target = the ref's primary).
        let resolver = other("B", "B");
        let got = replication_target("B", &cref("A", "1"), &resolver);
        assert_eq!(got, Some(("A".to_string(), PropagateDirection::Reverse)));
    }

    #[test]
    fn primary_no_backup_resolvable_is_none() {
        // Single-node / no peers: backup_resolver yields None on the primary path.
        let resolver = |_call_ref: &str| None;
        let got = replication_target("A", &cref("A", "1"), &resolver);
        assert_eq!(got, None, "no backup → store locally, no propagation");
    }

    #[test]
    fn legacy_ref_no_ordinal_is_owned_by_self() {
        // A ref with no encoded ordinal (pre-HA) is treated as self-owned →
        // primary path → forward to the backup.
        let resolver = other("A", "B");
        let got = replication_target("A", "legacy-no-ordinal", &resolver);
        assert_eq!(got, Some(("B".to_string(), PropagateDirection::Forward)));
    }

    #[test]
    fn plan_primary_path_stores_pri_self() {
        let resolver = other("A", "B");
        let plan = ReplicationPlan::resolve("A", &cref("A", "1"), &resolver);
        assert_eq!(plan.role, PartitionRole::Primary);
        assert_eq!(plan.primary, "A");
        assert_eq!(plan.target, Some(("B".to_string(), PropagateDirection::Forward)));
        let opts = plan.put_opts();
        assert_eq!(opts.peer.as_deref(), Some("B"));
        assert_eq!(opts.direction, Some(PropagateDirection::Forward));
    }

    #[test]
    fn plan_acting_backup_path_stores_bak_primary() {
        let resolver = other("B", "B");
        let plan = ReplicationPlan::resolve("B", &cref("A", "1"), &resolver);
        assert_eq!(plan.role, PartitionRole::Backup);
        assert_eq!(plan.primary, "A", "acting-backup stores under the original primary");
        assert_eq!(plan.target, Some(("A".to_string(), PropagateDirection::Reverse)));
        let opts = plan.put_opts();
        assert_eq!(opts.peer.as_deref(), Some("A"));
        assert_eq!(opts.direction, Some(PropagateDirection::Reverse));
    }

    #[test]
    fn plan_single_node_primary_local_only() {
        let resolver = |_call_ref: &str| None;
        let plan = ReplicationPlan::resolve("A", &cref("A", "1"), &resolver);
        assert_eq!(plan.role, PartitionRole::Primary);
        assert_eq!(plan.primary, "A");
        assert_eq!(plan.target, None);
        let opts = plan.put_opts();
        assert!(opts.peer.is_none(), "local-only: no propagation peer");
        assert!(opts.direction.is_none());
    }
}
