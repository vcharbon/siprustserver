//! The bulk reads of one partition of the replicating store: its call bodies
//! decoded, or its `callRef`s alone.

use super::envelope::{self, Enveloped};
use super::{CallState, CallStore, PartitionRole};

impl CallState {
    /// Decode every call body `(role, primary)` holds in the replicating store
    /// — this node's own `pri:{self}` for the bulk reclaim (ADR-0011 X11).
    /// The router materialises what it serves through the per-call read. Empty
    /// when no replicating store is wired. Each entry pairs the decoded call with
    /// its persisted clock-skew offset (`0` when the record carries none) — the
    /// router moves the call's instants into this node's frame by it before
    /// reading them (ADR-0055 §9). `beat` runs once
    /// per body read, so a caller's loop pulse shows a long scan making
    /// progress.
    pub async fn scan_partition(
        &self,
        role: PartitionRole,
        primary: &str,
        beat: &(dyn Fn() + Sync),
    ) -> Vec<(Enveloped, i64)> {
        let Some(repl) = self.repl_store() else {
            return Vec::new();
        };
        let bodies = repl.scan_calls(role, primary).await.unwrap_or_default();
        bodies
            .iter()
            .filter_map(|b| {
                beat();
                envelope::decode(&self.codec, b).ok()
            })
            .map(|c| {
                let skew = repl.skew_offset_ms(&c.call.call_ref).unwrap_or(0);
                (c, skew)
            })
            .collect()
    }

    /// The `callRef` of every call body `(role, primary)` holds, nothing decoded
    /// — a departed peer's `bak:{peer}` for the departure sweep (ADR-0047),
    /// which reads each body in turn. Empty when no replicating store is wired.
    pub fn partition_refs(&self, role: PartitionRole, primary: &str) -> Vec<String> {
        self.repl_store().map(|repl| repl.scan_call_refs(role, primary)).unwrap_or_default()
    }
}
