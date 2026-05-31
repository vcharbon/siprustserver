//! S10b — the **goal-2 simulated-failover harness** (plan "Goal-2 acceptance";
//! ADR-0011 X10). It composes, under ONE fake clock:
//!
//! - the `scenario-harness` SIP plane (alice/bob UAs + the SIP recorder),
//! - a real load-balancing `ProxyCore` SUT over a [`SimulatedWorkerRegistry`]
//!   (HRW pri/bak selection + signed Record-Route cookie + dead→backup routing),
//! - TWO replicating [`B2buaCore`] workers over a SHARED, *recording*
//!   [`SimulatedReplicationNetwork`] repl fabric + per-node [`SimulatedMembership`],
//! - a combined SIP + replication recording report.
//!
//! The canonical scenario (the must-pass test) is: alice → proxy → B1 establishes
//! a call that B1 replicates to B2; B1 crashes; an in-dialog request fails over to
//! B2 (acting-backup, reverse-propagates); B1 reboots EMPTY at a higher
//! incarnation gen, re-hydrates from B2, becomes ready; the next in-dialog message
//! routes back to B1 with the reclaimed, highest-gen state.
//!
//! ## Fake-clock discipline (CLAUDE.md hazards)
//! Everything runs under `#[tokio::test(start_paused = true)]`. [`FailoverHarness::advance`]
//! drives BOTH the SIP and replication sim pipelines with the proven
//! settle/advance/settle discipline. Both fabrics use transit delay `>= 1 ms`
//! (the SIP harness coerces 0→1; the repl fabric is built with 1). Drive the
//! protocol BETWEEN advances: advance to the deadline, then react. The cross-plane
//! pipeline is deep (txn → router → dispatcher → SIP net AND changelog → server →
//! delivery actor → puller → store), so [`settle`] yields generously.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{CdrRecord, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{Changelog, ReplicatingCallStore};
use b2bua::store::{CallStore, PartitionRole};
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};

use ha_harness::{Marker, ReplReport};
use repl_net::transport::{
    Fault, RecordingReplicationNetwork, ReplicationNetwork, SimulatedReplicationNetwork,
};
use scenario_harness::{Agent, Harness};
use sip_clock::Clock;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{
    LoadBalancerConfig, LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};
use sip_txn::IdGen;
use tokio::task::JoinHandle;
use topology::{Peer, SimulatedMembership};

/// Changelog TTLs `(tombstone, dead_peer)`: long enough that a backed-up call
/// survives the whole scenario, short enough that dead-peer auto-clean is
/// reachable in a test budget.
const DEFAULT_TTLS: (i64, i64) = (60_000, 600_000);

/// Let the whole cross-plane spawned pipeline hop forward. One yield only
/// advances one task hop and the pipeline is many hops deep across both planes,
/// so settle generously (matches the b2bua repl / ha-harness `settle`).
async fn settle() {
    for _ in 0..96 {
        tokio::task::yield_now().await;
    }
}

// ===========================================================================
// ReplicatedB2buaSut — a replicating B2BUA worker on the failover fabric
// ===========================================================================

/// The per-node repl wiring kept so a crashed node can be rebuilt in place
/// (same ordinal, same repl listen addr, fresh empty store, higher gen).
struct ReplWiring {
    /// Shared (recording) replication fabric every node listens/connects on.
    network: Arc<dyn ReplicationNetwork>,
    /// `ordinal → repl addr` for the supervisor's address resolver.
    addr_map: HashMap<String, SocketAddr>,
    /// The peers this node pulls from (its membership snapshot).
    peers: Vec<Peer>,
    /// This node's repl listen address (stable across reboots).
    listen_addr: SocketAddr,
    ttls: (i64, i64),
}

impl ReplWiring {
    /// Build the `ReplicationSetup` for a (re)spawn at incarnation `gen` over a
    /// fresh empty [`ReplicatingCallStore`].
    fn setup(&self, gen: u64, clock: &Clock) -> (ReplicationSetup, Arc<ReplicatingCallStore>) {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(self.ttls.0, self.ttls.1);
        let store = Arc::new(ReplicatingCallStore::with_changelog(
            changelog,
            clock.clone(),
        ));
        let membership: Arc<dyn topology::Membership> = Arc::new(SimulatedMembership::with_clock(
            self.peers.clone(),
            clock.clone(),
        ));
        let addr_map = self.addr_map.clone();
        let addr_resolver: Arc<dyn Fn(&Peer) -> SocketAddr + Send + Sync> =
            Arc::new(move |peer: &Peer| {
                *addr_map
                    .get(&peer.ordinal)
                    .unwrap_or_else(|| panic!("no repl addr for peer {}", peer.ordinal))
            });
        let setup = ReplicationSetup {
            network: self.network.clone(),
            membership,
            store: store.clone(),
            listen_addr: self.listen_addr,
            addr_resolver,
            incarnation_gen: gen,
        };
        (setup, store)
    }
}

/// A running replicating B2BUA worker bound on the failover harness's SIP fabric
/// and replicating over the shared repl fabric. Knows how to crash itself (abort
/// tasks + wipe store) and reboot itself (fresh empty store, higher gen, fresh
/// server/supervisor, same ordinal + same repl listen addr).
pub struct ReplicatedB2buaSut {
    ordinal: String,
    sip_addr: SocketAddr,
    /// The SIP endpoint factory — re-binding on reboot needs a fresh endpoint on
    /// the same address, so we keep the harness handle + name.
    sip_name: String,
    sip_bind: String,
    gen: u64,
    cdr: InMemoryCdrWriter,
    metrics: B2buaMetrics,
    /// The decision engine (routes every call to bob) — rebuilt on reboot.
    dest: (String, u16),
    /// The b-leg outbound proxy (so the worker's bob traffic traverses the proxy).
    outbound_proxy: Option<(String, u16)>,
    wiring: ReplWiring,
    clock: Clock,
    /// The live core (`None` only transiently between crash and reboot).
    core: Option<B2buaCore>,
    /// The repl store the live core uses (mirrors `core.repl_store()`).
    store: Arc<ReplicatingCallStore>,
    /// Handle to the harness so reboot can re-`bind_sut` on the same addr.
    harness: Arc<HarnessHandle>,
}

/// A shared handle to the `scenario_harness::Harness` so a worker can re-bind its
/// SIP endpoint on reboot (`Harness::bind_sut` takes `&self`). The `Harness` is
/// not `Sync` (its panic-dump guard holds a `Cell`), so we keep it behind a
/// `Mutex` and bind under a brief lock — making `Arc<HarnessHandle>` `Send + Sync`
/// for sharing across the SUTs. The whole harness runs on one (current-thread)
/// test task, so the lock is never contended.
pub struct HarnessHandle {
    inner: std::sync::Mutex<Option<Harness>>,
}

impl HarnessHandle {
    fn new(harness: Harness) -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(harness)),
        }
    }

    /// Bind a SUT endpoint on the shared fabric (under a brief lock). The future
    /// is awaited *after* the lock is released so the guard never crosses the
    /// `.await`.
    async fn bind_sut(&self, name: &str, addr: &str) -> (Box<dyn sip_net::UdpEndpoint>, SocketAddr) {
        // `bind_udp` is async; the harness is `!Sync`, so we cannot hold the
        // std Mutex guard across the await. `Harness::bind_sut` only registers a
        // lane (sync) + binds — but to keep the guard off the await boundary we
        // take the harness out, bind, then put it back.
        let h = self
            .inner
            .lock()
            .unwrap()
            .take()
            .expect("harness taken (already finished?)");
        let res = h.bind_sut(name, addr).await;
        *self.inner.lock().unwrap() = Some(h);
        res
    }
}

impl ReplicatedB2buaSut {
    /// This worker's cluster ordinal (== its proxy `WorkerId` == cookie `w_pri`
    /// when it is primary).
    pub fn ordinal(&self) -> &str {
        &self.ordinal
    }

    /// This worker's current incarnation gen (bumped on each reboot).
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// The CDR records this worker has written (call lifecycle assertions).
    pub fn cdr_records(&self) -> Vec<CdrRecord> {
        self.cdr.snapshot()
    }

    /// This worker's metrics (e.g. `creations_total` proves it processed/created
    /// a call locally — the load-bearing "handled on B2" signal after failover).
    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    /// Readiness gate (every reachable peer bootstrapped AND current). Drives the
    /// proxy registry health deterministically (vs. the OPTIONS probe loop).
    pub fn is_ready(&self) -> bool {
        self.core.as_ref().map(|c| c.is_ready()).unwrap_or(false)
    }

    /// Read a replicated body by `(role, primary, call_ref)` from this worker's
    /// repl store (introspection — assert a replica landed / was reclaimed).
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

    /// The live callRef KEYS this worker holds in `bak:{primary}` (the replicated
    /// partition for `primary`). Lets a test discover the replicated call's ref
    /// without re-deriving it from SIP state.
    pub fn scan_backed_up(&self, primary: &str) -> Vec<String> {
        self.store.scan_call_refs(PartitionRole::Backup, primary)
    }

    /// The first replicated callRef in `bak:{primary}`, or `None` if empty.
    pub async fn scan_one_backed_up(&self, primary: &str) -> Option<String> {
        self.scan_backed_up(primary).into_iter().next()
    }

    /// The live callRef KEYS this worker holds in `pri:{primary}` (its own
    /// authoritative partition / the reclaimed partition after reboot).
    pub fn scan_primary(&self, primary: &str) -> Vec<String> {
        self.store.scan_call_refs(PartitionRole::Primary, primary)
    }

    /// CRASH: abort the core's tasks + park its pullers, then drop it and replace
    /// the store with a fresh empty one (memory wiped). The node is inert until
    /// [`reboot`](Self::reboot). Closing the core's tasks closes its repl
    /// connections cleanly (the S9 note: crash-to-close, not a fabric partition).
    pub fn crash(&mut self) {
        if let Some(mut core) = self.core.take() {
            core.abort();
            // Dropping `core` releases the last store/supervisor `Arc`s it held.
        }
        // Wipe memory: a lingering `get` now sees an empty store at this gen.
        self.store = Arc::new(ReplicatingCallStore::new(self.gen, self.clock.clone()));
    }

    /// REBOOT: same ordinal + same repl listen addr, a fresh SIP endpoint, an
    /// EMPTY store at a NEW higher incarnation gen, a fresh server + supervisor →
    /// it re-bootstraps + resubscribes from its peers (the S6 reboot path). After
    /// driving the clock its [`is_ready`](Self::is_ready) flips true once
    /// re-hydration completes.
    pub async fn reboot(&mut self) {
        // Defensive: ensure any prior core is gone.
        if let Some(mut core) = self.core.take() {
            core.abort();
        }
        self.gen += 1;
        let (setup, store) = self.wiring.setup(self.gen, &self.clock);
        self.store = store;
        self.core = Some(self.spawn_core(Some(setup)).await);
    }

    /// (Re)bind the SIP endpoint and spawn a fresh `B2buaCore` over it with the
    /// given replication setup.
    async fn spawn_core(&self, replication: Option<ReplicationSetup>) -> B2buaCore {
        let (endpoint, _sa) = self.harness.bind_sut(&self.sip_name, &self.sip_bind).await;
        let cdr = self.cdr.clone();
        let config = B2buaConfig {
            self_ordinal: self.ordinal.clone(),
            sip_local_ip: self.sip_addr.ip().to_string(),
            sip_local_port: self.sip_addr.port(),
            b2b_outbound_proxy: self.outbound_proxy.clone(),
            ..Default::default()
        };
        let decision = Arc::new(ScriptedDecisionEngine::route_all_to(
            self.dest.0.clone(),
            self.dest.1,
        ));
        let deps = B2buaDeps {
            config,
            decision,
            limiter: Arc::new(NoopLimiter),
            cdr: Arc::new(cdr),
            // The legacy `store` slot is unused on the replicating path (the repl
            // store is the drain target); pass a throwaway in-memory store.
            store: Arc::new(b2bua::store::InMemoryCallStore::new()),
            clock: self.clock.clone(),
            id_gen: Arc::new(IdGen::seeded(0xB2B0 + self.gen)),
            replication,
        };
        B2buaCore::spawn(endpoint, deps)
    }
}

// ===========================================================================
// Proxy SUT — a real LoadBalancer over a SimulatedWorkerRegistry
// ===========================================================================

/// A running load-balancing proxy SUT + its driver-side registry handle, so the
/// scenario can flip a worker's health (dead on crash, alive+ready on recovery).
pub struct ProxySut {
    addr: SocketAddr,
    registry: SimulatedWorkerRegistry,
    metrics: Arc<ProxyMetrics>,
    task: JoinHandle<()>,
}

impl ProxySut {
    /// The proxy's listen address (alice/bob send through it).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy's metrics.
    pub fn metrics(&self) -> &Arc<ProxyMetrics> {
        &self.metrics
    }

    /// Mark a worker's health (e.g. `Dead` on crash, `Alive` on recovery).
    pub fn set_health(&self, ordinal: &str, health: WorkerHealth) {
        self.registry.set_health(ordinal, health);
    }
}

impl Drop for ProxySut {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ===========================================================================
// FailoverHarness — the orchestrator
// ===========================================================================

/// Ties the SIP plane (scenario-harness `Harness` + recorder), the repl plane
/// (recording-wrapped sim fabric), the proxy SUT, the two replicated workers,
/// alice/bob, and the shared clock into one fake-clock orchestration.
pub struct FailoverHarness {
    clock: Clock,
    /// Recording decorator over the repl sim fabric (the repl capture sink).
    repl_recording: RecordingReplicationNetwork,
    /// The underlying repl sim fabric — fault controls go here directly.
    repl_sim: Arc<SimulatedReplicationNetwork>,
    /// `ordinal → repl addr` (stable across reboots), for fault/lane mapping.
    repl_addrs: HashMap<String, SocketAddr>,
    /// Injected timeline markers (crash/reboot/failover/partition/…).
    markers: Vec<Marker>,
    /// The SIP harness handle (shared so workers can re-bind on reboot).
    harness: Arc<HarnessHandle>,
}

/// `127.0.0.1:9400+n` — a stable per-ordinal repl listen address.
fn repl_addr_for(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9400 + index as u16))
}

impl FailoverHarness {
    /// Build the harness over a fresh paused clock at t=0. `name` names the SIP
    /// recording; the SIP fabric uses a 1 ms transit delay (deterministic under a
    /// paused runtime). `worker_ordinals` declares the cluster up front so every
    /// node's repl addr-resolver map is complete before any worker spawns (the
    /// repl listen addr is assigned by declaration order and is stable across
    /// reboots).
    pub fn new(name: &str, worker_ordinals: &[&str]) -> Self {
        let clock = Clock::test_at(0);
        // SIP plane: a recording harness with 1 ms transit (0 is coerced anyway).
        let harness = Harness::with_transit_delay(name, 1).describe(
            "S10b goal-2 simulated failover: alice → proxy → 2 replicating b2buas \
             over the SIM SIP + SIM repl fabrics under one fake clock.",
        );
        // Repl plane: a recording-wrapped 1 ms sim fabric sharing the clock.
        let repl_sim = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let repl_recording = RecordingReplicationNetwork::new(
            repl_sim.clone() as Arc<dyn ReplicationNetwork>,
            clock.clone(),
        );
        let repl_addrs: HashMap<String, SocketAddr> = worker_ordinals
            .iter()
            .enumerate()
            .map(|(i, ord)| ((*ord).to_string(), repl_addr_for(i)))
            .collect();
        Self {
            clock,
            repl_recording,
            repl_sim,
            repl_addrs,
            markers: Vec::new(),
            harness: Arc::new(HarnessHandle::new(harness)),
        }
    }

    /// The shared clock (one timeline for both planes + assertions).
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// `now_ms()` off the shared clock.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Bind a named UA at `addr` on the SIP fabric (alice/bob).
    pub async fn agent(&self, name: &str, addr: &str) -> Agent {
        self.harness.agent(name, addr).await
    }

    /// Stand up the real load-balancing proxy SUT at `addr` over a
    /// [`SimulatedWorkerRegistry`] holding both workers (alive). HRW selection
    /// keys off Call-ID + the alive set; the scenario marks a worker dead/alive
    /// via [`ProxySut::set_health`].
    pub async fn spawn_proxy(&self, addr: &str, workers: &[(&str, SocketAddr)]) -> ProxySut {
        let entries: Vec<WorkerEntry> = workers
            .iter()
            .map(|(id, sa)| WorkerEntry::alive(*id, ProxyAddr::new(sa.ip().to_string(), sa.port())))
            .collect();
        let registry = SimulatedWorkerRegistry::with_clock(entries, self.clock.clone());
        let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
        let hmac =
            Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
        let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
            registry_dyn.clone(),
            hmac,
            observer,
            Arc::new(ProxyMetrics::new()),
            self.clock.clone(),
            LoadBalancerConfig::default(),
        ));

        let (ep, sock) = self.harness.bind_sut("proxy", addr).await;
        let metrics = Arc::new(ProxyMetrics::new());
        let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry_dyn)
            .clock(self.clock.clone())
            .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
            .metrics(metrics.clone())
            .build(ep);
        let task = tokio::spawn(core.run());
        ProxySut {
            addr: sock,
            registry,
            metrics,
            task,
        }
    }

    /// Bind a replicating B2BUA worker `ordinal` (declared in [`new`](Self::new))
    /// at SIP address `sip_bind`, with `peers` as its repl membership (every OTHER
    /// worker). It routes every call to `dest` and sends its b-leg through the
    /// proxy at `outbound_proxy`. Incarnation gen starts at 1. The repl listen
    /// addr + the full addr-resolver map come from the cluster declared in `new`.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_worker(
        &mut self,
        ordinal: &str,
        sip_name: &str,
        sip_bind: &str,
        peers: &[&str],
        dest: (&str, u16),
        outbound_proxy: (&str, u16),
    ) -> ReplicatedB2buaSut {
        let listen_addr = *self
            .repl_addrs
            .get(ordinal)
            .unwrap_or_else(|| panic!("worker {ordinal} was not declared in FailoverHarness::new"));
        let sip_addr: SocketAddr = sip_bind.parse().expect("sip addr");

        // The full addr-resolver map (covers this node + every peer) is known up
        // front from the declared cluster — no post-spawn linking needed.
        let addr_map = self.repl_addrs.clone();
        let peer_list: Vec<Peer> = peers.iter().map(|p| Peer::new(*p, *p)).collect();
        let wiring = ReplWiring {
            network: Arc::new(self.repl_recording.clone()) as Arc<dyn ReplicationNetwork>,
            addr_map,
            peers: peer_list,
            listen_addr,
            ttls: DEFAULT_TTLS,
        };

        let cdr = InMemoryCdrWriter::new();
        let mut sut = ReplicatedB2buaSut {
            ordinal: ordinal.to_string(),
            sip_addr,
            sip_name: sip_name.to_string(),
            sip_bind: sip_bind.to_string(),
            gen: 1,
            cdr: cdr.clone(),
            metrics: B2buaMetrics::new(),
            dest: (dest.0.to_string(), dest.1),
            outbound_proxy: Some((outbound_proxy.0.to_string(), outbound_proxy.1)),
            wiring,
            clock: self.clock.clone(),
            core: None,
            store: Arc::new(ReplicatingCallStore::new(1, self.clock.clone())),
            harness: self.harness.clone(),
        };
        let (setup, store) = sut.wiring.setup(1, &self.clock);
        sut.store = store;
        let core = sut.spawn_core(Some(setup)).await;
        sut.metrics = core.metrics().clone();
        sut.core = Some(core);
        sut
    }

    // -- markers / fabric controls ----------------------------------------

    /// Inject a timeline marker stamped with the current clock.
    pub fn mark(&mut self, node: &str, peer: Option<&str>, kind: &str, detail: &str) {
        self.markers.push(Marker {
            at_ms: self.clock.now_ms(),
            node: node.to_string(),
            peer: peer.map(|p| p.to_string()),
            kind: kind.to_string(),
            detail: detail.to_string(),
        });
    }

    /// Partition two workers on the repl fabric (cut both directions). Marker.
    pub fn partition(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_addrs[a], self.repl_addrs[b]);
        self.repl_sim.apply_fault(Fault::Partition { a: aa, b: ba });
        self.mark(a, Some(b), "partition", "");
    }

    /// Heal a repl-fabric partition. Marker.
    pub fn heal(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_addrs[a], self.repl_addrs[b]);
        self.repl_sim.apply_fault(Fault::Heal { a: aa, b: ba });
        self.mark(a, Some(b), "heal", "");
    }

    // -- clock -------------------------------------------------------------

    /// Advance the paused clock by `dur`, driving BOTH the SIP and repl sim
    /// pipelines with the proven settle/advance/settle discipline (CLAUDE.md).
    /// Drive the protocol BETWEEN advances: advance to the deadline, then assert.
    pub async fn advance(&self, dur: Duration) {
        let ms = dur.as_millis() as u64;
        let chunks = ms.div_ceil(100).max(1);
        for _ in 0..chunks {
            settle().await;
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
        // Trailing pass so frames/messages staged in the last chunk also land.
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }

    // -- report ------------------------------------------------------------

    /// Snapshot the replication recording (captured frames + markers + lanes).
    pub fn repl_report(&self) -> ReplReport {
        let lanes: BTreeMap<SocketAddr, String> = self
            .repl_addrs
            .iter()
            .map(|(ord, addr)| (*addr, ord.clone()))
            .collect();
        ReplReport {
            frames: self.repl_recording.captured(),
            markers: self.markers.clone(),
            lanes,
        }
    }

    /// Render the COMBINED report: the SIP exchange (scenario-harness text
    /// renderer) AND the replication exchange (the S9 ha-harness renderer)
    /// together, with crash/reboot/failover markers, into one string. Consumes
    /// the SIP harness to close the recording (call last).
    pub async fn report(self) -> String {
        let repl = self.repl_report();
        let repl_text = repl.render_text();
        let repl_mmd = repl.render_mermaid();

        // Close the SIP recording → projected SIP entries.
        let run = self.harness.close_for_report().await;
        let sip_text = render_sip_text(&run);

        let mut out = String::new();
        out.push_str("══════════════════════════════════════════════════════════════════════\n");
        out.push_str("  S10b GOAL-2 SIMULATED FAILOVER — combined SIP + replication report\n");
        out.push_str("══════════════════════════════════════════════════════════════════════\n\n");
        out.push_str("── SIP plane (alice / proxy / b2bua / bob) ───────────────────────────\n");
        out.push_str(&sip_text);
        out.push('\n');
        out.push_str("── Replication plane (changelog pull / data / bootstrap) ─────────────\n");
        out.push_str(&repl_text);
        out.push_str("\n── Replication plane (mermaid sequenceDiagram) ───────────────────────\n");
        out.push_str(&repl_mmd);
        out
    }

    /// Render the combined report and write it under `dir` as
    /// `failover.txt` (combined) + `replication.mmd` (the repl mermaid).
    /// Returns the written paths. Consumes the harness.
    pub async fn write_report(self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let combined = self.report().await;
        let txt = dir.join("failover.txt");
        std::fs::write(&txt, &combined)?;
        Ok(vec![txt])
    }
}

impl HarnessHandle {
    /// Bind a named UA on the shared fabric (alice/bob), under a brief lock.
    async fn agent(&self, name: &str, addr: &str) -> Agent {
        let h = self
            .inner
            .lock()
            .unwrap()
            .take()
            .expect("harness taken (already finished?)");
        let a = h.agent(name, addr).await;
        *self.inner.lock().unwrap() = Some(h);
        a
    }

    /// Close the SIP recording and return the projected report (for rendering).
    /// Takes the harness out of the shared handle and consumes it. The caller
    /// must have dropped the SUTs first (so no further `bind_sut` races this).
    async fn close_for_report(&self) -> scenario_harness::RunReport {
        let h = self
            .inner
            .lock()
            .unwrap()
            .take()
            .expect("close_for_report: harness already taken/finished");
        h.finish().await
    }
}

/// Render a compact SIP sequence from the projected run report (one line per
/// delivered message: `t=<ms> from → to  METHOD/STATUS`). Reuses the report's
/// own entries (recording-first); no interpreter state.
fn render_sip_text(run: &scenario_harness::RunReport) -> String {
    let entries = run.entries();
    let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!("messages: {}\n", entries.len()));
    out.push_str(&"-".repeat(70));
    out.push('\n');
    for e in entries {
        let label = scenario_harness::report::wire::facets(&e.raw).label;
        let undelivered = if e.delivered { "" } else { "  [UNDELIVERED]" };
        out.push_str(&format!(
            "t={:>6} {} -> {}  {}{}\n",
            e.sent_ms as i64 - base,
            e.from,
            e.to,
            label,
            undelivered,
        ));
    }
    out
}
