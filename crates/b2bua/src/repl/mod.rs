//! `repl` — the b2bua HA replication layer (ADR-0011): the store + changelog;
//! the serve-loop ([`ReplServer`]), the per-peer client FSM ([`Puller`]), and
//! the topology-driven [`ReplicationSupervisor`] — forward Replog tailing; and
//! Bootstrap re-hydration: the server's lazy-batch `bak:{caller}` KEYS scan
//! ([`ReplServer`]), the puller's `Bootstrapping` state + the hard-timer
//! backstop ([`Puller`]), and the supervisor's
//! `bootstrap_complete`/`all_bootstrapped` readiness signal
//! ([`ReplicationSupervisor`]). Readiness/OPTIONS lives in `readiness`.
//!
//! - [`Changelog`] — node-global monotonic counter over per-peer compacted
//!   ref-logs (the in-process `propagate:{peer}` ZSET equivalent). Bodies are
//!   read live from the store at drain time (ADR-0011 X3); deletes leave a
//!   TTL-reaped tombstone; dead peers auto-clean.
//! - [`ReplicatingCallStore`] — a [`CallStore`](crate::store::CallStore) that
//!   stores `Arc<[u8]>` bodies (ADR-0011 X8), honours the HA params the in-memory
//!   impl no-ops (`peer`/`direction`/`call_gen`/`ttl`), and **atomically bumps
//!   the changelog** on every mutation.

mod changelog;
mod incarnation;
mod puller;
mod readiness;
mod replication;
mod resurrection;
mod self_endpoint;
mod server;
mod shed_marks;
mod store;
mod supervisor;

pub use changelog::{
    BodySource, Changelog, RefMeta, DEFAULT_DEAD_PEER_TTL_MS, DEFAULT_TOMBSTONE_TTL_MS,
};
pub use puller::{Puller, PullerConfig, PullerStatus};
pub use readiness::{Readiness, ReadinessSource, ReadinessState};
pub use replication::{flush_replicated, replication_target, ReplicationPlan};
pub use self_endpoint::{SelfEndpoint, WithdrawalCondition};
pub use server::ReplServer;
pub use store::ReplicatingCallStore;
pub use supervisor::{AddrResolver, FnPeerResolver, PeerLink, PeerResolver, ReplicationSupervisor};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod idle_wait_tests;

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
mod s12_tests;

#[cfg(test)]
mod s13_tests;

#[cfg(test)]
mod real_transport_tests;
