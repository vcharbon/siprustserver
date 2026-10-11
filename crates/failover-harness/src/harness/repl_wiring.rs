//! [`ReplWiring`] — what a worker needs to (re)build its replication setup
//! on the shared fabric, kept so a crashed node can be rebuilt in place.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use b2bua::repl::{Changelog, PullerConfig, ReplicatingCallStore, ServeConfig};
use b2bua::ReplicationSetup;
use repl_net::transport::{ListenError, ReplicationListener, ReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

/// The pullers' backoff ceiling: a cut that outlasts the idle bound leaves its
/// streams (ADR-0045), and a heal reaches the pullers within this.
const HEAL_REACH_MS: u64 = 1_000;

/// The per-node repl wiring kept so a crashed node can be rebuilt in place
/// (same ordinal, same repl listen addr, fresh empty store, higher gen).
pub(super) struct ReplWiring {
    /// Shared (recording) replication fabric every node listens/connects on.
    pub(super) network: Arc<dyn ReplicationNetwork>,
    /// The cluster's LIVE `ordinal → repl addr` map, shared by every node: the
    /// resolver reads it per connect attempt (ADR-0012 D3), so re-pointing an
    /// ordinal at a replacement's listen address reaches the running peers.
    pub(super) addr_map: Arc<Mutex<HashMap<String, SocketAddr>>>,
    /// The peers this node pulls from (its membership snapshot).
    pub(super) peers: Vec<Peer>,
    /// This node's repl listen address (stable across reboots).
    pub(super) listen_addr: SocketAddr,
    pub(super) ttls: (i64, i64),
}

impl ReplWiring {
    /// Build the `ReplicationSetup` for a (re)spawn at incarnation `gen` over a
    /// fresh empty [`ReplicatingCallStore`]. Also returns the concrete
    /// [`SimulatedMembership`] handle so a scenario can drive membership deltas
    /// (e.g. remove a killed peer, the way k8s drops a dead pod's endpoint — the
    /// signal the survivor's supervisor turns into an eager takeover).
    pub(super) async fn setup(
        &self,
        gen: u64,
        clock: &Clock,
    ) -> (ReplicationSetup, Arc<ReplicatingCallStore>, Arc<SimulatedMembership>) {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(self.ttls.0, self.ttls.1);
        let store = Arc::new(ReplicatingCallStore::with_changelog(changelog, clock.clone()));
        let sim_membership =
            Arc::new(SimulatedMembership::with_clock(self.peers.clone(), clock.clone()));
        let membership: Arc<dyn topology::Membership> = sim_membership.clone();
        let addr_map = self.addr_map.clone();
        // The resolver is now async (ADR-0012 D3); wrap the cluster's live
        // ordinal→addr map in the sync-closure adapter. It is READ per connect
        // attempt, so a peer that moved (a replacement on a fresh listen addr)
        // is reached on the next reconnect without respawning anything.
        let addr_resolver: b2bua::repl::AddrResolver =
            Arc::new(b2bua::repl::FnPeerResolver(move |peer: &Peer| {
                *addr_map
                    .lock()
                    .unwrap()
                    .get(&peer.ordinal)
                    .unwrap_or_else(|| panic!("no repl addr for peer {}", peer.ordinal))
            }));
        let setup = ReplicationSetup {
            network: self.network.clone(),
            membership,
            store: store.clone(),
            listener: self.bind().await,
            addr_resolver,
            incarnation_gen: gen,
            puller: PullerConfig { backoff_max_ms: HEAL_REACH_MS, ..PullerConfig::default() },
            serve: ServeConfig::default(),
        };
        (setup, store, sim_membership)
    }

    /// Bind the listen address. A crashed incarnation's listener is dropped
    /// when the runtime next polls its aborted task, as a dead process's socket
    /// closes after it exits: the bind yields until then.
    async fn bind(&self) -> Box<dyn ReplicationListener> {
        for _ in 0..64 {
            match self.network.listen(self.listen_addr).await {
                Ok(listener) => return listener,
                Err(ListenError::AlreadyInUse(_)) => tokio::task::yield_now().await,
                Err(e) => panic!("bind the repl listener at {}: {e}", self.listen_addr),
            }
        }
        panic!("the repl listen address {} stays bound by a live incarnation", self.listen_addr)
    }
}
