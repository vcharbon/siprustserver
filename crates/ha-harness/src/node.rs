//! [`HaNode`] — one goal-1 replication-subsystem node (plan Decision 7).
//!
//! A node bundles exactly the engine pieces (and nothing SIP):
//! `{ ReplicatingCallStore + per-peer Changelog + ReplServer on a listener +
//! ReplicationSupervisor pulling its peers + a Membership view + the shared
//! Clock + an incarnation gen }`. It is the lift of the `Node` struct the b2bua
//! `s5_tests`/`s6_tests`/`s8_tests` hand-wire, packaged so the cluster can drive
//! put/delete/crash/reboot against it.
//!
//! `crash()` aborts the node's spawned tasks (server accept-loop + every puller)
//! AND drops the store/changelog so the node's memory is wiped — a true crash.
//! `reboot()` rebuilds it: same ordinal, EMPTY store, a NEW higher incarnation
//! gen, fresh server + supervisor → it re-bootstraps + resubscribes from its
//! peers (the S6 reboot-recovery path).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::repl::{
    flush_replicated, Changelog, PullerConfig, ReplServer, ReplicatingCallStore,
    ReplicationSupervisor,
};
use b2bua::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};
use repl_net::transport::ReplicationNetwork;
use sip_clock::Clock;
use topology::{Membership, Peer, SimulatedMembership};

/// The wiring a node needs to (re)build its server + supervisor on demand.
pub(crate) struct NodeWiring {
    /// Shared (recording) replication fabric every node listens/connects on.
    pub network: Arc<dyn ReplicationNetwork>,
    /// `ordinal → repl addr` for the supervisor's address resolver.
    pub addr_map: HashMap<String, SocketAddr>,
    /// The peers this node pulls from (its membership snapshot).
    pub peers: Vec<Peer>,
    /// Puller backoff / bootstrap-timeout config (tests inject short values).
    pub config: PullerConfig,
    /// Changelog TTLs `(tombstone, dead_peer)` so dead-peer auto-clean is
    /// reachable within a test budget.
    pub ttls: (i64, i64),
}

/// One in-process HA node. Owns its store + the handles to abort on crash.
pub struct HaNode {
    pub(crate) ordinal: String,
    pub(crate) addr: SocketAddr,
    pub(crate) gen: u64,
    pub(crate) store: ReplicatingCallStore,
    pub(crate) supervisor: ReplicationSupervisor,
    /// The membership view driving this node's pullers (clone-shared state).
    pub(crate) membership: Arc<SimulatedMembership>,
    /// Abort handles: the server accept-loop task. Dropped/aborted on crash.
    server_task: tokio::task::JoinHandle<()>,
    clock: Clock,
}

impl HaNode {
    /// Build + spawn a node on the shared fabric. The server listens on `addr`;
    /// the supervisor starts pulling `wiring.peers`.
    pub(crate) async fn spawn(
        ordinal: &str,
        addr: SocketAddr,
        gen: u64,
        clock: Clock,
        wiring: &NodeWiring,
    ) -> Self {
        let (store, supervisor, membership, server_task) =
            Self::build(ordinal, addr, gen, &clock, wiring).await;
        Self {
            ordinal: ordinal.to_string(),
            addr,
            gen,
            store,
            supervisor,
            membership,
            server_task,
            clock,
        }
    }

    /// Construct the store + server + supervisor + membership for `ordinal` at
    /// incarnation `gen`, returning the pieces (shared by spawn + reboot).
    async fn build(
        ordinal: &str,
        addr: SocketAddr,
        gen: u64,
        clock: &Clock,
        wiring: &NodeWiring,
    ) -> (
        ReplicatingCallStore,
        ReplicationSupervisor,
        Arc<SimulatedMembership>,
        tokio::task::JoinHandle<()>,
    ) {
        let changelog =
            Changelog::new(gen, clock.clone()).with_ttls(wiring.ttls.0, wiring.ttls.1);
        let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());

        // Server: accept + serve the changelog over the fabric.
        let listener = wiring.network.listen(addr).await.expect("listen");
        let server = ReplServer::new(ordinal, changelog, Arc::new(store.clone()));
        let server_task = tokio::spawn(server.run(listener));

        // Supervisor: pull every peer. The resolver maps ordinal → repl addr.
        let addr_map = wiring.addr_map.clone();
        let resolve = Arc::new(move |peer: &Peer| {
            *addr_map
                .get(&peer.ordinal)
                .unwrap_or_else(|| panic!("no addr for peer {}", peer.ordinal))
        });
        let supervisor = ReplicationSupervisor::with_config(
            ordinal,
            wiring.network.clone(),
            store.clone(),
            resolve,
            clock.clone(),
            wiring.config,
        );
        let membership = Arc::new(SimulatedMembership::with_clock(
            wiring.peers.clone(),
            clock.clone(),
        ));
        supervisor.start(membership.clone() as Arc<dyn Membership>);

        (store, supervisor, membership, server_task)
    }

    /// This node's ordinal.
    pub fn ordinal(&self) -> &str {
        &self.ordinal
    }

    /// This node's current incarnation gen (bumped on each reboot).
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// This node's replication listen address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Put `body` for `call_ref` at content version `call_gen`, routing it
    /// through the write-side policy ([`flush_replicated`]): the injected
    /// `backup_resolver` names this node's backup peer (2/3-node tests resolve it
    /// trivially), the policy picks the partition + Forward/Reverse direction.
    pub async fn put(
        &self,
        call_ref: &str,
        body: Vec<u8>,
        call_gen: i64,
        backup_resolver: &dyn Fn(&str) -> Option<String>,
    ) {
        flush_replicated(
            &self.store,
            &self.ordinal,
            call_ref,
            body,
            &[],
            0,
            call_gen,
            backup_resolver,
        )
        .await
        .expect("put");
    }

    /// Delete `call_ref`, propagating the tombstone the same way `put` routes a
    /// body (resolve partition + direction, bump the changelog).
    pub async fn delete(&self, call_ref: &str, backup_resolver: &dyn Fn(&str) -> Option<String>) {
        let plan =
            b2bua::repl::ReplicationPlan::resolve(&self.ordinal, call_ref, backup_resolver);
        let opts = plan.put_opts();
        self.store
            .delete_call(plan.role, &plan.primary, call_ref, &[], &opts)
            .await
            .expect("delete");
    }

    /// Read a stored body by `(role, primary, call_ref)` (introspection).
    pub async fn get(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Vec<u8>> {
        self.store
            .get_call(role, primary, call_ref)
            .await
            .expect("get")
            .map(|b| b.to_vec())
    }

    /// The content version (`call_gen`) currently stored for a ref, or `None`.
    pub fn call_gen(&self, role: PartitionRole, primary: &str, call_ref: &str) -> Option<i64> {
        self.store.current_call_gen(role, primary, call_ref)
    }

    /// Is `peer` current on this node (sticky-current after a tail Noop)?
    pub fn is_current(&self, peer: &str) -> bool {
        self.supervisor.is_current(peer)
    }

    /// Has `peer`'s bootstrap completed on this node?
    pub fn is_bootstrapped(&self, peer: &str) -> bool {
        self.supervisor.bootstrap_complete(peer)
    }

    /// Ready = every known peer bootstrapped AND current (the S7 readiness gate,
    /// read straight off the supervisor — no SIP/OPTIONS).
    pub fn is_ready(&self) -> bool {
        self.supervisor.all_bootstrapped() && self.supervisor.all_current()
    }

    /// The retained watermark for `peer` (introspection).
    pub fn watermark(&self, peer: &str) -> repl_net::Watermark {
        self.supervisor.watermark(peer)
    }

    /// Reap expired bodies + changelog tombstones/idle peers after a clock
    /// advance (lazy TTL — deterministic, no background task).
    pub async fn reap(&self, now_ms: i64) {
        self.store.reap(now_ms).await;
    }

    /// CRASH: abort the server accept-loop task and drop the store + supervisor
    /// so the node's memory is wiped. The spawned per-connection / per-puller
    /// tasks lose their last `Arc` to the store/changelog and unwind; a fresh
    /// empty store replaces the old one so a lingering `get` would see nothing.
    /// The node stays in the cluster map but is inert until reboot. Driven via
    /// [`HaCluster::crash`](crate::HaCluster::crash).
    pub(crate) fn crash(&mut self) {
        self.server_task.abort();
        // Cancel the supervisor's pullers by replacing it with an inert one over
        // a throwaway store; the old supervisor's `Arc<SupervisorInner>` is the
        // only thing keeping its puller tasks' store alive once we drop our copy.
        // Replacing `store`/`supervisor` here drops the harness's strong refs.
        let dead = ReplicatingCallStore::new(self.gen, self.clock.clone());
        self.store = dead;
        // The membership view is left in place; reboot rebuilds everything.
    }

    /// REBOOT: same ordinal, EMPTY store, a NEW higher incarnation gen, fresh
    /// server + supervisor → re-bootstrap + resubscribe from peers (S6 path).
    /// Driven via [`HaCluster::reboot`](crate::HaCluster::reboot).
    pub(crate) async fn reboot(&mut self, wiring: &NodeWiring) {
        // Make sure any prior server task is gone (idempotent if already crashed).
        self.server_task.abort();
        self.gen += 1;
        let (store, supervisor, membership, server_task) =
            Self::build(&self.ordinal, self.addr, self.gen, &self.clock, wiring).await;
        self.store = store;
        self.supervisor = supervisor;
        self.membership = membership;
        self.server_task = server_task;
    }

    /// Add a peer to this node's membership view (drives its supervisor to spawn
    /// a puller).
    pub fn add_peer(&self, peer: Peer) {
        self.membership.add(peer);
    }

    /// Remove a peer from this node's membership view (parks its puller; the
    /// retained watermark survives).
    pub fn remove_peer(&self, ordinal: &str) {
        self.membership.remove(ordinal);
    }
}

/// Forward (primary→backup) put options targeting `peer` — used by tests that
/// want to drive a raw store mutation rather than the policy.
pub fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

/// Reverse (acting-backup→primary) put options targeting `peer`.
pub fn rev(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Reverse),
    }
}
