//! [`HaCluster`] — the goal-1 cluster harness: N in-process nodes over one
//! shared, **recording** simulated replication fabric + a shared paused
//! [`Clock`], with put/delete/crash/reboot/partition drivers + convergence
//! assertions + the recording report.
//!
//! ## Fake-clock discipline (CLAUDE.md hazards — lifted from the b2bua repl tests)
//! Everything runs under `#[tokio::test(start_paused = true)]`. [`advance`] drives
//! the deep sim pipeline (notify → server drain → send → delivery actor → puller
//! recv → store apply → status publish) with the proven tick/settle pattern:
//! settle, advance one 100 ms chunk, settle — repeated, with a trailing
//! advance+settle so frames staged in the last chunk also land. Transit delay is
//! `>= 1 ms` (the fabric coerces 0 → 1). Drive the protocol BETWEEN advances:
//! `advance` to the deadline you want to trip, then assert.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::repl::PullerConfig;
use repl_net::transport::{
    Fault, RecordingReplicationNetwork, ReplicationNetwork, SimulatedReplicationNetwork,
};
use sip_clock::Clock;
use topology::Peer;

use crate::node::{HaNode, NodeWiring};
use crate::report::{Marker, ReplReport};

/// Default changelog TTLs for harness nodes: short enough that dead-peer
/// auto-clean is reachable in a test budget, long enough that steady-state
/// tombstones survive a few advances. `(tombstone_ms, dead_peer_ms)`.
const DEFAULT_TTLS: (i64, i64) = (30_000, 300_000);

/// Short backoff + bootstrap hard timeout so a couple of advances trip the
/// relevant deadline deterministically (mirrors the b2bua repl tests' fast cfg).
fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

/// `127.0.0.1:9300+n` — a stable per-ordinal repl address.
fn addr_for(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9300 + index as u16))
}

/// The N-node goal-1 cluster. Holds the shared fabric + clock + the node map.
pub struct HaCluster {
    clock: Clock,
    /// The recording decorator (capture sink) wrapping the sim fabric.
    recording: RecordingReplicationNetwork,
    /// The underlying sim fabric — fault controls go here directly.
    sim: Arc<SimulatedReplicationNetwork>,
    /// `ordinal → node`.
    nodes: BTreeMap<String, HaNode>,
    /// `ordinal → repl addr` (stable across reboots).
    addrs: HashMap<String, SocketAddr>,
    /// Injected timeline markers for the report (crash/reboot/partition/…).
    markers: Vec<Marker>,
    config: PullerConfig,
    ttls: (i64, i64),
    /// Backstop body-TTL for replicas stored with `ttl_ms <= 0` (long by default;
    /// split-brain tests inject a short value via [`with_replica_backstop_ms`]).
    ///
    /// [`with_replica_backstop_ms`]: HaCluster::with_replica_backstop_ms
    replica_backstop_ms: i64,
}

/// Default replica backstop: long enough not to evict steady-state replicas in
/// any existing test (which advance seconds), matching the store default.
const DEFAULT_REPLICA_BACKSTOP_MS: i64 = 3_600_000;

impl HaCluster {
    /// Build an N-node cluster from `node_ordinals` with a fresh paused clock at
    /// t=0 and a 1 ms-transit recording fabric. Every node lists every OTHER
    /// node as a peer (the natural goal-1 full mesh); pass `with_clock` /
    /// `with_peers` builders if a custom topology is needed.
    pub async fn new(node_ordinals: &[&str]) -> Self {
        Self::with_clock(node_ordinals, Clock::test_at(0)).await
    }

    /// [`new`](Self::new) with a caller-supplied clock (share one timeline with
    /// other harness pieces).
    pub async fn with_clock(node_ordinals: &[&str], clock: Clock) -> Self {
        Self::build_cluster(node_ordinals, clock, DEFAULT_REPLICA_BACKSTOP_MS).await
    }

    /// [`with_clock`](Self::with_clock) with an explicit replica backstop TTL
    /// (split-brain tests inject a short value so a missed-delete ghost evicts
    /// within the test budget while live replicas are kept alive by re-puts).
    pub async fn with_replica_backstop_ms(
        node_ordinals: &[&str],
        clock: Clock,
        replica_backstop_ms: i64,
    ) -> Self {
        Self::build_cluster(node_ordinals, clock, replica_backstop_ms).await
    }

    async fn build_cluster(
        node_ordinals: &[&str],
        clock: Clock,
        replica_backstop_ms: i64,
    ) -> Self {
        let sim = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let recording =
            RecordingReplicationNetwork::new(sim.clone() as Arc<dyn ReplicationNetwork>, clock.clone());

        let mut addrs = HashMap::new();
        for (i, ord) in node_ordinals.iter().enumerate() {
            addrs.insert((*ord).to_string(), addr_for(i));
        }

        let mut cluster = Self {
            clock,
            recording,
            sim,
            nodes: BTreeMap::new(),
            addrs,
            markers: Vec::new(),
            config: fast_config(),
            ttls: DEFAULT_TTLS,
            replica_backstop_ms,
        };

        // Spawn each node with a full-mesh peer list (every other ordinal).
        for ord in node_ordinals {
            let peers: Vec<Peer> = node_ordinals
                .iter()
                .filter(|o| *o != ord)
                .map(|o| Peer::new(*o, *o))
                .collect();
            let wiring = cluster.wiring_for(peers);
            let addr = cluster.addrs[*ord];
            let node = HaNode::spawn(ord, addr, 1, cluster.clock.clone(), &wiring).await;
            cluster.nodes.insert((*ord).to_string(), node);
        }
        cluster
    }

    /// The shared clock (share one timeline with external pieces / assertions).
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// `now_ms()` off the shared clock (for marker stamping in tests).
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Build the per-node wiring (network = the recording fabric, addr map,
    /// peers, config, ttls) the node uses to (re)build its server+supervisor.
    fn wiring_for(&self, peers: Vec<Peer>) -> NodeWiring {
        NodeWiring {
            network: Arc::new(self.recording.clone()) as Arc<dyn ReplicationNetwork>,
            addr_map: self.addrs.clone(),
            peers,
            config: self.config,
            ttls: self.ttls,
            replica_backstop_ms: self.replica_backstop_ms,
        }
    }

    /// Immutable node accessor (introspection).
    pub fn node(&self, ordinal: &str) -> &HaNode {
        self.nodes
            .get(ordinal)
            .unwrap_or_else(|| panic!("no node {ordinal}"))
    }

    /// Mutable node accessor (crash/reboot).
    pub fn node_mut(&mut self, ordinal: &str) -> &mut HaNode {
        self.nodes
            .get_mut(ordinal)
            .unwrap_or_else(|| panic!("no node {ordinal}"))
    }

    /// Every node ordinal, sorted (deterministic order).
    pub fn ordinals(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    // -- mutations ---------------------------------------------------------

    /// Put on `ordinal` via the write-side policy, recording a `put` marker. The
    /// `backup_resolver` names the node's backup peer (2/3-node tests resolve it
    /// trivially).
    pub async fn put(
        &mut self,
        ordinal: &str,
        call_ref: &str,
        body: Vec<u8>,
        call_gen: i64,
        call_bgen: i64,
        backup_resolver: &dyn Fn(&str) -> Option<String>,
    ) {
        self.mark(ordinal, None, "put", &format!("{call_ref} cv=({call_gen},{call_bgen})"));
        self.node(ordinal)
            .put(call_ref, body, call_gen, call_bgen, backup_resolver)
            .await;
    }

    /// Delete on `ordinal` via the write-side policy, recording a `delete` marker.
    pub async fn delete(
        &mut self,
        ordinal: &str,
        call_ref: &str,
        backup_resolver: &dyn Fn(&str) -> Option<String>,
    ) {
        self.mark(ordinal, None, "delete", call_ref);
        self.node(ordinal).delete(call_ref, backup_resolver).await;
    }

    /// Crash `ordinal`: abort its tasks + wipe its memory. Records a marker.
    pub fn crash(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "crash", "");
        self.node_mut(ordinal).crash();
    }

    /// Reboot `ordinal`: empty store, higher gen, fresh server+supervisor →
    /// re-bootstrap. Records a marker carrying the new gen.
    pub async fn reboot(&mut self, ordinal: &str) {
        let peers: Vec<Peer> = self
            .ordinals()
            .into_iter()
            .filter(|o| o != ordinal)
            .map(|o| Peer::new(o.clone(), o))
            .collect();
        let wiring = self.wiring_for(peers);
        self.node_mut(ordinal).reboot(&wiring).await;
        let new_gen = self.node(ordinal).gen();
        self.mark(ordinal, None, "reboot", &format!("gen={new_gen}"));
    }

    // -- fabric controls ---------------------------------------------------

    /// Partition two nodes (cut both directions + block reconnect). Marker.
    pub fn partition(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.addrs[a], self.addrs[b]);
        self.sim.apply_fault(Fault::Partition { a: aa, b: ba });
        self.mark(a, Some(b), "partition", "");
    }

    /// Heal a partition between two nodes. Marker.
    pub fn heal(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.addrs[a], self.addrs[b]);
        self.sim.apply_fault(Fault::Heal { a: aa, b: ba });
        self.mark(a, Some(b), "heal", "");
    }

    /// Cut one direction `from → to`. Marker.
    pub fn cut(&mut self, from: &str, to: &str) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::Cut { src: fa, dst: ta });
        self.mark(from, Some(to), "cut", "");
    }

    /// Raise the transit delay `from → to`.
    pub fn delay(&mut self, from: &str, to: &str, ms: u64) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::Delay { src: fa, dst: ta, ms });
        self.mark(from, Some(to), "delay", &format!("{ms}ms"));
    }

    /// Stall delivery `from → to` (buffer grows, nothing delivered).
    pub fn stall(&mut self, from: &str, to: &str) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::Stall { src: fa, dst: ta });
        self.mark(from, Some(to), "stall", "");
    }

    /// Resume a stalled OR blocked direction `from → to` (buffered frames flush
    /// in order; a [`block`](Self::block)ed writer unblocks).
    pub fn resume(&mut self, from: &str, to: &str) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::Resume { src: fa, dst: ta });
        self.mark(from, Some(to), "resume", "");
    }

    /// Black-hole `from → to`: the peer stops pulling, so `send` BLOCKS once the
    /// in-flight window fills (a hung/half-open peer that never resets). No error,
    /// no close — cleared by [`resume`](Self::resume). Marker.
    pub fn block(&mut self, from: &str, to: &str) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::Block { src: fa, dst: ta });
        self.mark(from, Some(to), "block", "");
    }

    /// Fail `from → to` with a network error after `ms`: an established `send`
    /// returns an Io error, `recv` closes (reset), and a fresh connect on the pair
    /// is rejected — modelling ECONNRESET some time into the connection. Marker.
    pub fn error_after(&mut self, from: &str, to: &str, ms: u64) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim.apply_fault(Fault::ErrorAfter { src: fa, dst: ta, ms });
        self.mark(from, Some(to), "error_after", &format!("{ms}ms"));
    }

    /// Arm buffer-overflow → drop-subscriber on `from → to`.
    pub fn drop_on_overflow(&mut self, from: &str, to: &str) {
        let (fa, ta) = (self.addrs[from], self.addrs[to]);
        self.sim
            .apply_fault(Fault::DropOnOverflow { src: fa, dst: ta });
        self.mark(from, Some(to), "drop_on_overflow", "");
    }

    /// `reconnect(a,b)` — heal the pair so a fresh connect succeeds (an alias for
    /// [`heal`](Self::heal) named per the slice API; recorded as `reconnect`).
    pub fn reconnect(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.addrs[a], self.addrs[b]);
        self.sim.apply_fault(Fault::Heal { a: aa, b: ba });
        self.mark(a, Some(b), "reconnect", "");
    }

    // -- clock -------------------------------------------------------------

    /// Advance the paused clock by `dur`, driving the deep sim pipeline with the
    /// proven settle/advance/settle discipline (CLAUDE.md). Drive the protocol
    /// BETWEEN advances: advance to the deadline, then assert.
    pub async fn advance(&self, dur: Duration) {
        let ms = dur.as_millis() as u64;
        let chunks = ms.div_ceil(100).max(1);
        for _ in 0..chunks {
            settle().await;
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
        // Trailing pass so frames produced during the last settle (e.g. a
        // just-woken server drain) get their transit timer tripped + delivered.
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }

    // -- report ------------------------------------------------------------

    /// Snapshot the recording: captured frames + injected markers + lane map.
    pub fn report(&self) -> ReplReport {
        let lanes: BTreeMap<SocketAddr, String> = self
            .addrs
            .iter()
            .map(|(ord, addr)| (*addr, ord.clone()))
            .collect();
        ReplReport {
            frames: self.recording.captured(),
            markers: self.markers.clone(),
            lanes,
        }
    }

    /// Render the text + mermaid reports and write them under `dir` as
    /// `replication.txt` and `replication.mmd`. Returns the written paths.
    pub fn write_report(&self, dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let report = self.report();
        let txt = dir.join("replication.txt");
        let mmd = dir.join("replication.mmd");
        std::fs::write(&txt, report.render_text())?;
        std::fs::write(&mmd, report.render_mermaid())?;
        Ok(vec![txt, mmd])
    }

    /// Inject a timeline marker stamped with the current clock.
    fn mark(&mut self, node: &str, peer: Option<&str>, kind: &str, detail: &str) {
        self.markers.push(Marker {
            at_ms: self.clock.now_ms(),
            // Standalone ha-harness cluster: no shared SIP sequencer, so the
            // marker's global seq is unused (the repl-only renderer orders by a
            // local append counter). The unified failover combiner is the only
            // consumer of `seq`, and it supplies a real sequencer.
            seq: 0,
            node: node.to_string(),
            peer: peer.map(|p| p.to_string()),
            kind: kind.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// Let the whole spawned pipeline hop forward. One yield only advances one task
/// hop and the pipeline is several hops deep, so settle generously (matches the
/// b2bua repl tests' `settle`).
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}
