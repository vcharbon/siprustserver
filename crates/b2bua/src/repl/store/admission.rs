//! Which write a [`ReplicatingCallStore`] takes, under its metadata lock: the
//! resurrection tombstones, the call incarnation a `(p,b)` version orders, and
//! an adopted version against the one held.

use std::collections::HashMap;
use std::sync::Arc;

use super::super::incarnation::is_another_call;
use super::super::resurrection::Burial;
use super::{CallMeta, ReplicatingCallStore};
use crate::store::{locked, PartitionRole, PropagateDirection};

/// What a write is, for the rules of [`ReplicatingCallStore::admits`].
#[derive(Clone, Copy)]
pub(super) struct Write {
    pub(super) role: PartitionRole,
    pub(super) direction: Option<PropagateDirection>,
    pub(super) now: i64,
    /// The `(p,b)` the write carries.
    pub(super) version: (i64, i64),
    /// The version is one this node adopted ([`PutOpts::adopted`]).
    pub(super) adopted: bool,
}

impl ReplicatingCallStore {
    /// The incarnation rule of a write of `incarnation` of `call_ref`, under
    /// the metadata lock (ADR-0014, "`(p,b)` orders one call incarnation"), and
    /// the version rule of an adopted write. `false` leaves the store as it is:
    ///
    /// - an adopted version of the call the store holds at that version or past
    ///   it is the other owner's to keep: its body at that version is the one
    ///   held (ADR-0031 D3);
    /// - a buried call stays buried (the resurrection guard); a write of a call
    ///   another one replaced re-stamps its burial, and a backup keeps it for
    ///   the replica reap;
    /// - an acting backup's write (`Reverse`) never replaces another call: the
    ///   authority replaced the call it serves, so the call is buried as
    ///   replaced and its body kept for the reap.
    ///
    /// `true` stores the write. When it replaces another call, that call's
    /// record, body and index keys go first and the call is buried as
    /// replaced; a backup keeps its body for the reap when the body is a
    /// version a backup authored (`b > 0`).
    pub(super) fn admits(
        &self,
        meta: &mut HashMap<String, CallMeta>,
        call_ref: &str,
        incarnation: Option<&str>,
        body: &Arc<[u8]>,
        write: Write,
    ) -> bool {
        let mut tombstones = locked(&self.tombstones, "resurrection tombstones");
        let keep_for_reap = |incarnation: Option<&str>, body: &Arc<[u8]>| {
            if let Some(id) = incarnation {
                locked(&self.displaced, "displaced bodies").insert(id.to_string(), body.clone());
            }
        };
        match tombstones.burial(call_ref, incarnation, write.now) {
            Some(Burial::Deleted) => return false,
            Some(Burial::Replaced) => {
                // A copy still writing the replaced call keeps it buried for as
                // long as it writes, not just the window after the replacement.
                if let Some(id) = incarnation {
                    tombstones.bury(call_ref, Some(id.to_string()), Burial::Replaced, write.now);
                }
                if write.role == PartitionRole::Backup {
                    keep_for_reap(incarnation, body);
                }
                return false;
            }
            None => {}
        }
        let held = meta.get(call_ref).and_then(|m| m.meta.incarnation.clone());
        if !is_another_call(held.as_deref(), incarnation) {
            let (p, b) = write.version;
            let holds = |m: &CallMeta| m.meta.call_gen >= p && m.meta.call_bgen >= b;
            return !(write.adopted && meta.get(call_ref).is_some_and(holds));
        }
        if write.direction == Some(PropagateDirection::Reverse) {
            let owned = incarnation.map(str::to_string);
            tombstones.bury(call_ref, owned, Burial::Replaced, write.now);
            if write.role == PartitionRole::Backup {
                keep_for_reap(incarnation, body);
            }
            return false;
        }
        if let Some(gone) = meta.remove(call_ref) {
            self.account_role(Some(gone.role), None);
            let removed =
                self.inner.remove_body(gone.role, &gone.primary, call_ref, &gone.meta.indexes);
            tombstones.bury(call_ref, gone.meta.incarnation.clone(), Burial::Replaced, write.now);
            // Only a version a backup authored (`b > 0`) can be a terminal it
            // deferred; the authority settles its own (`b == 0`).
            let backup_authored = gone.meta.call_bgen > 0;
            if let (PartitionRole::Backup, true, Some(removed)) =
                (gone.role, backup_authored, removed)
            {
                keep_for_reap(gone.meta.incarnation.as_deref(), &removed);
            }
        }
        true
    }
}
