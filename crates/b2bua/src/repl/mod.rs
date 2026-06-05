//! `repl` — the b2bua HA replication layer (ADR-0011). S4 is the store +
//! changelog; **S5** adds the serve-loop ([`ReplServer`]), the per-peer client
//! FSM ([`Puller`]), and the topology-driven [`ReplicationSupervisor`] —
//! forward Replog tailing. **S6** adds Bootstrap re-hydration: the server's
//! lazy-batch `bak:{caller}` KEYS scan ([`ReplServer`]), the puller's
//! `Bootstrapping` state + the hard-timer backstop ([`Puller`]), and the
//! supervisor's `bootstrap_complete`/`all_bootstrapped` readiness signal
//! ([`ReplicationSupervisor`]). Readiness/OPTIONS is S7.
//!
//! - [`Changelog`] — node-global monotonic counter over per-peer compacted
//!   ref-logs (the in-process `propagate:{peer}` ZSET equivalent). Bodies are
//!   read live from the store at drain time (Decision 2); deletes leave a
//!   TTL-reaped tombstone; dead peers auto-clean.
//! - [`ReplicatingCallStore`] — a [`CallStore`](crate::store::CallStore) that
//!   stores `Arc<[u8]>` bodies (Decision 9), honours the HA params the in-memory
//!   impl no-ops (`peer`/`direction`/`call_gen`/`ttl`), and **atomically bumps
//!   the changelog** on every mutation.

mod changelog;
mod puller;
mod readiness;
mod replication;
mod server;
mod store;
mod supervisor;

pub use changelog::{
    BodySource, Changelog, RefMeta, DEFAULT_DEAD_PEER_TTL_MS, DEFAULT_TOMBSTONE_TTL_MS,
};
pub use puller::{Puller, PullerConfig, PullerState, PullerStatus};
pub use readiness::{Readiness, ReadinessSource, ReadinessState};
pub use replication::{flush_replicated, replication_target, ReplicationPlan};
pub use server::ReplServer;
pub use store::ReplicatingCallStore;
pub use supervisor::{AddrResolver, FnPeerResolver, PeerResolver, ReplicationSupervisor};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod s5_tests;

#[cfg(test)]
mod s6_tests;

#[cfg(test)]
mod s7_tests;

#[cfg(test)]
mod s8_tests;

#[cfg(test)]
mod s10_tests;

#[cfg(test)]
mod s11_tests;

#[cfg(test)]
mod real_transport_tests;
