//! `ha-harness` — the **goal-1 pure-HA-framework test harness** (ADR-0011 X10 /
//! plan Decision 7).
//!
//! It packages the already-tested b2bua replication engine
//! (`crates/b2bua/src/repl`, slices S4–S8) into a reusable N-node cluster
//! harness: in-process replication-subsystem nodes — `{ ReplicatingCallStore +
//! per-peer Changelog + sim ReplicationNetwork + ReplServer + Puller /
//! ReplicationSupervisor + Membership view + Clock + incarnation gen }` — with
//! **no SIP, no router, no rules**. It drives put/delete/crash/reboot/partition,
//! asserts convergence, and emits a **recording-first** replication-exchange
//! report (ADR-0006).
//!
//! It does NOT reinvent the engine — it lifts the canonical node wiring from the
//! b2bua `s5_tests`/`s6_tests`/`s8_tests` (store + changelog + ReplServer on a
//! listener + supervisor pulling peers over a shared `SimulatedReplicationNetwork`
//! + `SimulatedMembership`, with the `tick`/`settle` fake-clock helpers).
//!
//! ## Entry points
//! - [`HaCluster`] — owns the shared recording fabric + clock + node map; the
//!   builder ([`HaCluster::new`] / [`HaCluster::with_clock`]), the fault controls
//!   (`partition`/`heal`/`cut`/`delay`/`stall`/`resume`/`drop_on_overflow`/
//!   `reconnect`), the `advance` driver, and `report`/`write_report`.
//! - [`HaNode`] — one node: `put`/`delete`/`crash`/`reboot` + introspection
//!   (`get`/`call_gen`/`is_current`/`is_ready`/`watermark`).
//! - [`ReplReport`] — the recording snapshot + the focused text/mermaid renderer.
//!
//! ## Fake-clock discipline
//! Every test runs under `#[tokio::test(start_paused = true)]`; drive the
//! protocol BETWEEN [`HaCluster::advance`]s (advance to the deadline, then
//! assert). See CLAUDE.md hazards — transit `>= 1 ms`, settle around advances.

mod cluster;
mod node;
mod report;

pub use cluster::HaCluster;
pub use node::{fwd, rev, HaNode};
pub use report::{frame_summary, Marker, ReplReport};

// Re-export the engine value types tests touch so a consumer needs only this
// crate for ordinary scenarios.
pub use b2bua::store::{PartitionRole, PropagateDirection};
pub use repl_net::transport::{CapturedFrame, Direction};
pub use repl_net::{Frame, Op, Partition, Watermark};
pub use topology::Peer;

/// Build a callRef whose encoded primary is `primary` (so `partition_of` / the
/// write policy route it): `"{primary}|{id}|t{id}"`.
pub fn cref(primary: &str, id: &str) -> String {
    format!("{primary}|{id}|t{id}")
}

/// A trivial backup resolver that names a single fixed peer as the backup —
/// the 2-node test convention ("my backup is the other node").
pub fn backup_is(peer: &'static str) -> impl Fn(&str) -> Option<String> {
    move |_call_ref: &str| Some(peer.to_string())
}

/// A backup resolver that names no backup (single-node / local-only path).
pub fn no_backup(_call_ref: &str) -> Option<String> {
    None
}
