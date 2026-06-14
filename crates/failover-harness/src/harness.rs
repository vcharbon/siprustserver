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
use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{Changelog, ReplicatingCallStore};
use b2bua::store::{CallStore, PartitionRole};
use b2bua::{B2buaCore, ReplicationSetup};
use b2bua_harness::B2buaSpawnParams;

use ha_harness::{Marker, ReplReport};
use repl_net::transport::{
    Fault, RecordingReplicationNetwork, ReplicationNetwork, SimulatedReplicationNetwork,
};
use scenario_harness::{Agent, Harness};
use sip_clock::Clock;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
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
    /// fresh empty [`ReplicatingCallStore`]. Also returns the concrete
    /// [`SimulatedMembership`] handle so a scenario can drive membership deltas
    /// (e.g. remove a killed peer, the way k8s drops a dead pod's endpoint — the
    /// signal the survivor's supervisor turns into an eager takeover).
    fn setup(
        &self,
        gen: u64,
        clock: &Clock,
    ) -> (ReplicationSetup, Arc<ReplicatingCallStore>, Arc<SimulatedMembership>) {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(self.ttls.0, self.ttls.1);
        let store = Arc::new(ReplicatingCallStore::with_changelog(
            changelog,
            clock.clone(),
        ));
        let sim_membership = Arc::new(SimulatedMembership::with_clock(
            self.peers.clone(),
            clock.clone(),
        ));
        let membership: Arc<dyn topology::Membership> = sim_membership.clone();
        let addr_map = self.addr_map.clone();
        // The resolver is now async (ADR-0012 D3); wrap the sim's ordinal→addr map
        // in the sync-closure adapter.
        let addr_resolver: b2bua::repl::AddrResolver =
            Arc::new(b2bua::repl::FnPeerResolver(move |peer: &Peer| {
                *addr_map
                    .get(&peer.ordinal)
                    .unwrap_or_else(|| panic!("no repl addr for peer {}", peer.ordinal))
            }));
        let setup = ReplicationSetup {
            network: self.network.clone(),
            membership,
            store: store.clone(),
            listen_addr: self.listen_addr,
            addr_resolver,
            incarnation_gen: gen,
        };
        (setup, store, sim_membership)
    }
}

/// A running replicating B2BUA worker bound on the failover harness's SIP fabric
/// and replicating over the shared repl fabric. Knows how to crash itself (abort
/// tasks + wipe store) and reboot itself (fresh empty store, higher gen, fresh
/// server/supervisor, same ordinal + same repl listen addr).
pub struct ReplicatedB2buaSut {
    ordinal: String,
    /// The LIVE incarnation's SIP wire addr. A reboot moves this to a fresh
    /// address (new pod IP) — see [`reboot`](Self::reboot) / [`reboot_sip_addr`].
    sip_addr: SocketAddr,
    /// The gen-1 SIP addr, kept stable so each incarnation's reboot address is a
    /// deterministic function of `(base, gen)` rather than compounding.
    sip_base_addr: SocketAddr,
    /// The SIP endpoint factory — re-binding on reboot needs a fresh endpoint, so
    /// we keep the harness handle + name. `sip_bind` tracks the live addr.
    sip_name: String,
    sip_bind: String,
    gen: u64,
    cdr: InMemoryCdrWriter,
    metrics: B2buaMetrics,
    /// The default route destination (kept for reference; the live decision is
    /// stored in `decision`, which a limiter scenario may override).
    #[allow(dead_code)]
    dest: (String, u16),
    /// The b-leg outbound proxy (so the worker's bob traffic traverses the proxy).
    outbound_proxy: Option<(String, u16)>,
    wiring: ReplWiring,
    clock: Clock,
    /// The live core (`None` only transiently between crash and reboot).
    core: Option<B2buaCore>,
    /// The repl store the live core uses (mirrors `core.repl_store()`).
    store: Arc<ReplicatingCallStore>,
    /// This node's concrete membership handle, so a scenario can inject deltas
    /// (e.g. [`simulate_peer_removed`](Self::simulate_peer_removed)). Replaced on
    /// each (re)spawn — a survivor that never reboots keeps its initial handle.
    membership: Arc<SimulatedMembership>,
    /// Handle to the harness so reboot can re-`bind_sut` on the same addr.
    harness: Arc<HarnessHandle>,
    /// Decision engine (shared across reboots). Default routes every call to
    /// `dest`; a limiter scenario supplies one carrying `call_limiter` entries.
    decision: Arc<dyn CallDecisionEngine>,
    /// Call limiter (shared across reboots). Default `NoopLimiter`; a limiter
    /// scenario supplies an `HttpCallLimiter` over the shared HTTP fabric.
    limiter: Arc<dyn CallLimiter>,
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
        // Delegate to the LIVE core's metrics. `B2buaCore::spawn` mints its own
        // `B2buaMetrics` (deps carry none), so the SUT's own `self.metrics` field
        // is never written by the core — reading it gave a permanent 0 for the
        // X11 reclaim/handback counters under test. Fall back to the (empty) field
        // only while crashed.
        self.core
            .as_ref()
            .map(|c| c.metrics())
            .unwrap_or(&self.metrics)
    }

    /// Readiness gate (every reachable peer bootstrapped AND current). Drives the
    /// proxy registry health deterministically (vs. the OPTIONS probe loop).
    pub fn is_ready(&self) -> bool {
        self.core.as_ref().map(|c| c.is_ready()).unwrap_or(false)
    }

    /// **Ground-truth** live in-memory call count (the actual `inner.calls` map
    /// size), bypassing the `creations − removals` counters. The X11 reclaim/
    /// handback accounting is exactly what's under test, so assertions key on this
    /// rather than the metric. `0` while crashed.
    pub fn active_calls(&self) -> usize {
        self.core.as_ref().map(|c| c.active_calls()).unwrap_or(0)
    }

    /// Live per-call serialization-lock count (`inner.locks.len()`). Should track
    /// [`active_calls`](Self::active_calls); a residue after a call ends is the
    /// orphan-reject lock leak — an in-dialog request that 481'd on the acting
    /// backup / rebooted primary without releasing its per-call state. `0` while
    /// crashed.
    pub fn lock_count(&self) -> usize {
        self.core.as_ref().map(|c| c.lock_count()).unwrap_or(0)
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

    /// The primary version counter (`p`) currently stored for a ref, or `None`
    /// — projected from the `(p,b)` version vector ([`current_cv`]).
    pub fn call_gen(&self, role: PartitionRole, primary: &str, call_ref: &str) -> Option<i64> {
        self.store.current_cv(role, primary, call_ref).map(|(p, _)| p)
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

    // ── High-level HA concepts (ADR-0014) ───────────────────────────────────────
    // Failover tests assert on these, NOT on low-level constructs (partition
    // bodies, per-call locks, repl counters). The vocabulary is the cluster's:
    // who *serves* a call, whether a backup is *synchronized* (holds a current
    // replica it could take over from), and whether a node's *memory is clean*
    // (no per-call state left behind).

    /// Does this node currently **serve** `call_ref` — hold it live, so it would
    /// emit the call's keepalive and answer in-dialog traffic? The cluster
    /// invariant is "exactly one node serves a given call" (see
    /// [`assert_single_owner`](crate::assert_single_owner)). `false` while crashed.
    pub fn serves(&self, call_ref: &str) -> bool {
        self.core.as_ref().map(|c| c.serves(call_ref)).unwrap_or(false)
    }

    /// HARNESS SURGERY (see `B2buaCore::drop_live_copy`): drop the live
    /// in-memory copy of `call_ref` with NO store mutation — the deterministic
    /// recreation of the rebooted-primary "imported into `pri:{self}` but not
    /// yet materialised" mid-reclaim state, which the bulk-`ReclaimAll` race
    /// only yields under timing.
    pub fn drop_live_copy(&self, call_ref: &str) -> bool {
        self.core.as_ref().map(|c| c.drop_live_copy(call_ref)).unwrap_or(false)
    }

    /// Is this node **synchronized** as the backup for `call_ref` — does it hold a
    /// current replica it could take the call over from? (The primary is encoded in
    /// `call_ref`; this reads the `bak:{primary}` partition.) The behavioural twin
    /// is "the owner answers 200 on a probe OPTIONS" (drive a keepalive); this is
    /// the at-rest "the backup could take over" check.
    pub async fn is_synchronized_backup(&self, call_ref: &str) -> bool {
        match call::parse_call_ref(call_ref) {
            Some(p) => self
                .store
                .get_call(PartitionRole::Backup, &p.primary, call_ref)
                .await
                .ok()
                .flatten()
                .is_some(),
            None => false,
        }
    }

    /// Has this node **cleaned up all per-call memory** — no live calls and no
    /// per-call serialization locks left behind? The high-level "no leak" check
    /// (replaces poking `active_calls()`/`lock_count()` directly). `true` while
    /// crashed (an empty node holds nothing).
    pub fn memory_clean(&self) -> bool {
        self.active_calls() == 0 && self.lock_count() == 0
    }

    /// Does this node hold **any trace** of `call_ref` — live (serving) or as a
    /// replica body in either partition? Used to assert a terminated call left
    /// nothing behind anywhere (so a later reboot cannot resurrect it).
    pub async fn holds_any_trace(&self, call_ref: &str) -> bool {
        if self.serves(call_ref) {
            return true;
        }
        match call::parse_call_ref(call_ref) {
            Some(p) => {
                for role in [PartitionRole::Primary, PartitionRole::Backup] {
                    if self
                        .store
                        .get_call(role, &p.primary, call_ref)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        return true;
                    }
                }
                false
            }
            None => false,
        }
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

    /// Drive a `MemberDelta::Removed` for `ordinal` into THIS node's membership —
    /// the simulation of k8s dropping a killed pod's endpoint from the survivor's
    /// view. The node's supervisor reconciles it to a Park. Under reactive-only
    /// takeover (ADR-0014) this no longer drives an eager takeover (removed); the
    /// survivor takes a dialog over only when the proxy reroutes its in-dialog
    /// traffic. Kept so the survivor's membership view stays honest across a kill.
    pub fn simulate_peer_removed(&self, ordinal: &str) {
        self.membership.remove(ordinal);
    }

    /// Drive a `MemberDelta::Added` for `ordinal` into THIS node's membership —
    /// the simulation of k8s re-publishing a restarted pod's endpoint. The
    /// survivor's supervisor re-spawns its puller to the peer (seeded from the
    /// retained watermark) → fresh forward replication. A statefulset restart is
    /// observed as Removed-then-Added; pair this with a prior
    /// [`simulate_peer_removed`](Self::simulate_peer_removed). Host == ordinal,
    /// matching the harness's `Peer::new(p, p)` convention.
    pub fn simulate_peer_added(&self, ordinal: &str) {
        self.membership.add(Peer::new(ordinal, ordinal));
    }

    /// The SIP address a reboot at the current `gen` binds on. Models a new pod
    /// IP: same port, a gen-stamped HOST (`127.0.<gen>.1`) so the reborn worker
    /// shares NO network endpoint with the dead incarnation — in-flight SIP toward
    /// the old IP is undeliverable, and the proxy must re-learn the address (via
    /// `registry.set_address`, the real k8s-EndpointSlice path) before in-dialog
    /// traffic routes to it again. Deterministic in `(base_port, gen)`.
    fn reboot_sip_addr(&self) -> SocketAddr {
        let octet = u8::try_from(self.gen).expect("gen fits a host octet (< 256 reboots)");
        SocketAddr::from(([127, 0, octet, 1], self.sip_base_addr.port()))
    }

    /// REBOOT: same ordinal + same repl listen addr, a fresh SIP endpoint on a
    /// NEW address (new pod IP — [`reboot_sip_addr`](Self::reboot_sip_addr)), an
    /// EMPTY store at a NEW higher incarnation gen, a fresh server + supervisor →
    /// it re-bootstraps + resubscribes from its peers (the S6 reboot path).
    /// Returns the new SIP address so the caller re-learns it into the proxy
    /// registry (and the report). After driving the clock its
    /// [`is_ready`](Self::is_ready) flips true once re-hydration completes.
    ///
    /// PRISTINE GUARANTEE (endurance invariant): aborting the prior core drops its
    /// `TimerService` (every per-call timer dies) and its SIP endpoint; the store
    /// is replaced with a fresh empty one. This method then HARD-ASSERTS the
    /// reborn node holds nothing — no live calls, no per-call locks — BEFORE any
    /// reclaim re-hydrates it. If a future change ever lets call context or a timer
    /// survive the wipe, this trips here at reboot, not three hours into endurance.
    pub async fn reboot(&mut self) -> SocketAddr {
        // Defensive: ensure any prior core is gone. Abort drops its TimerService +
        // SIP endpoint; the `store` swap below frees the prior store/changelog.
        if let Some(mut core) = self.core.take() {
            core.abort();
        }
        self.gen += 1;
        // New pod IP: rebind on a fresh address so there is no network continuity
        // with the dead incarnation. Update both the typed addr and the bind str
        // BEFORE `spawn_core` (which reads `sip_addr` for the worker's own config
        // and `sip_bind` for `bind_sut`).
        let new_addr = self.reboot_sip_addr();
        self.sip_addr = new_addr;
        self.sip_bind = new_addr.to_string();
        let (setup, store, membership) = self.wiring.setup(self.gen, &self.clock);
        self.store = store;
        self.membership = membership;
        self.core = Some(self.spawn_core(Some(setup)).await);

        // Pristine BEFORE reclaim: no settle/advance has run since spawn, so the
        // supervisor's bootstrap pull has not materialised anything yet.
        assert_eq!(
            self.active_calls(),
            0,
            "rebooted {} must come up with zero live calls (pristine restart invariant)",
            self.ordinal,
        );
        assert_eq!(
            self.lock_count(),
            0,
            "rebooted {} must come up with zero per-call locks (pristine restart invariant)",
            self.ordinal,
        );
        new_addr
    }

    /// (Re)bind the SIP endpoint and spawn a fresh `B2buaCore` over it with the
    /// given replication setup.
    async fn spawn_core(&self, replication: Option<ReplicationSetup>) -> B2buaCore {
        let (endpoint, _sa) = self.harness.bind_sut(&self.sip_name, &self.sip_bind).await;
        let params = B2buaSpawnParams {
            ordinal: self.ordinal.clone(),
            sip_addr: self.sip_addr,
            decision: self.decision.clone(),
            limiter: self.limiter.clone(),
            // No callflow services on the replicating path (the plain `spawn`
            // path before was `spawn_with_services(.., vec![])`).
            services: Vec::new(),
            outbound_proxy: self.outbound_proxy.clone(),
            replication,
            clock: self.clock.clone(),
            id_gen: Arc::new(IdGen::seeded(0xB2B0 + self.gen)),
            cdr: self.cdr.clone(),
        };
        b2bua_harness::spawn_b2bua_core(endpoint, params, |config| {
            // EXACT production (kind) timers — `deploy/k8s/manifests/20-worker.yaml`.
            // The keepalive cells must be representative: a long quiescent call is
            // flushed (and its backup TTL refreshed) only by its in-dialog OPTIONS,
            // so the dead-peer/limiter-refresh/backup-TTL cadence only matches
            // production at the real 300 s interval. Under a `start_paused` clock
            // advancing 300 s costs nothing in wall-time, so every cell pays the
            // full interval. `reboot_budget_sec` (600 s) ≥ `keepalive_interval_sec`
            // (300 s) keeps the backup TTL alive across one keepalive gap
            // (config.rs validate). `keepalive_timeout_sec` (45 s, B2BUA_KEEPALIVE_
            // TIMEOUT_SEC) is the reboot-recovery grace before a reclaimed dialog's
            // re-armed OPTIONS is declared dead — the code default (32 s) is NOT the
            // cluster value, so set it explicitly here for parity.
            config.keepalive_interval_sec = 300;
            config.keepalive_timeout_sec = 45;
            config.reboot_budget_sec = 600;
            // Disable the RFC 3261 §13.3.1.4 un-ACKed-2xx watchdog in this harness:
            // the `confirmed_pre_ack` matrix cells deliberately hold the dialog in
            // the pre-ACK window for longer than the 1 s 2xx-retransmit cadence, so
            // the watchdog would emit retransmits that the strict differential
            // transparency oracle (baseline vs failover, token-for-token) is not
            // designed to align — they are pure noise for what this harness tests
            // (SIP-transparent failover), exercised instead by the dedicated
            // `unacked_2xx_reap` b2bua-harness test.
            config.ack_timeout_sec = 0;
        })
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
    /// The real OPTIONS health-probe loop (ADR-0012). `Some` when the proxy was
    /// stood up via [`FailoverHarness::spawn_proxy_with_health_probe`]: health is
    /// then driven by actual probe replies (200→Alive, 503 not-ready→NotReady)
    /// instead of `set_health`, so a rebooted worker's Unknown→NotReady→Alive
    /// lifecycle — the state the response-path reverse-failover branches on — is
    /// exercised for real. Aborted on drop.
    probe_task: Option<JoinHandle<()>>,
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

    /// Re-learn a worker's address — the proxy's k8s-EndpointSlice path. On reboot
    /// a pod returns at a NEW IP, and in-dialog routing only follows it once the
    /// registry resolves the worker ordinal (carried in the signed Record-Route
    /// cookie) to the new address. Without this, a cookie-routed in-dialog request
    /// — e.g. the 200 coming back for the rebooted worker's own keepalive OPTIONS —
    /// would target the dead address and be lost.
    pub fn set_address(&self, ordinal: &str, addr: SocketAddr) {
        self.registry
            .set_address(ordinal, ProxyAddr::new(addr.ip().to_string(), addr.port()));
    }

    /// The proxy's CURRENT health view of a worker (as the registry holds it).
    /// When a health probe is running this reflects real probe replies; callers
    /// poll it to wait for a rebooted worker to be re-confirmed `Alive` by the
    /// probe (rather than asserting it via `set_health`).
    pub fn health(&self, ordinal: &str) -> Option<WorkerHealth> {
        self.registry.resolve(ordinal).map(|w| w.health)
    }
}

impl Drop for ProxySut {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(p) = self.probe_task.take() {
            p.abort();
        }
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
    /// The scenario name passed to [`new`](Self::new) — also the write-on-Drop
    /// artifact subdir (sanitized).
    name: String,
    /// Armed write-on-Drop flag (mirrors `scenario-harness`'s `PanicDump`). When
    /// still armed at Drop the harness renders the unified report into
    /// `target/seq-reports/<sanitized name>/report.{html,global.txt,replication.mmd}`.
    /// Every explicit-report path (`run_cell`, `report`, `write_report`,
    /// `write_unified_report`) disarms it so we never double-write. A `Cell` (not
    /// atomic) is enough — the whole harness lives on one current-thread test task.
    report_on_drop: std::cell::Cell<bool>,
    /// Recording decorator over the repl sim fabric (the repl capture sink).
    repl_recording: RecordingReplicationNetwork,
    /// The underlying repl sim fabric — fault controls go here directly.
    repl_sim: Arc<SimulatedReplicationNetwork>,
    /// `ordinal → repl addr` (stable across reboots), for fault/lane mapping.
    repl_addrs: HashMap<String, SocketAddr>,
    /// `ordinal → LATEST SIP wire addr`, so the unified report's combiner can
    /// collapse a worker's SIP + repl + lifecycle rows onto one column. A reboot
    /// re-binds the worker on a NEW address (new pod IP), so this is updated on
    /// reboot to the live incarnation's addr.
    worker_sip_addrs: HashMap<String, SocketAddr>,
    /// EVERY worker SIP addr ever bound, across all incarnations. The
    /// endpoint-scoped RFC CSeq audit excludes worker binds (a transparent
    /// failover splits one dialog's CSeq stream across workers, which the audit
    /// would misread as a skip); after a reboot moves a worker to a new addr its
    /// PRE-reboot bind must stay excluded too, so accumulate rather than replace.
    all_worker_sip_addrs: Vec<SocketAddr>,
    /// Injected timeline markers (crash/reboot/failover/partition/…).
    markers: Vec<Marker>,
    /// The ONE shared global recording-order sequencer (the SIP recorder's
    /// `EventSequencer`). Markers are stamped from it at the instant of
    /// `mark()`/`partition()`/`heal()`/crash/reboot so they interleave with SIP
    /// messages and repl frames in true append order (Issue 1).
    event_seq: Arc<layer_harness::EventSequencer>,
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
        // The generic scenario-Harness CSeq gate is UNSCOPED (audits every bind,
        // including the internal cluster workers). A transparent failover splits
        // one dialog's CSeq stream across workers, which that gate misreads as a
        // skip. `FailoverHarness` runs its own endpoint-scoped audit on Drop
        // (`rfc_audit_findings`), so disarm the redundant unscoped one here.
        harness.disarm_cseq_gate();
        // ONE shared global recording-order sequencer for ALL THREE planes. It IS
        // the SIP layer-harness `EventSequencer` (the same counter that stamps
        // every recorded SIP message's `seq`); we thread it into the repl capture
        // sink and into the lifecycle/chaos markers so the unified combiner can
        // render strictly in TRUE append order. `at_ms` then serves only as the
        // displayed time label — never the cross-source tiebreaker — so a reboot
        // marker appended just before the bootstrap pull it triggers sorts first
        // even though both land on the same paused-clock millisecond (Issue 1).
        let event_seq = harness.recording().recorder().sequencer();
        let capture_seq: repl_net::transport::CaptureSeq = {
            let s = event_seq.clone();
            Arc::new(move || s.next())
        };
        // Repl plane: a recording-wrapped 1 ms sim fabric sharing the clock AND
        // the global sequencer.
        let repl_sim = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let repl_recording = RecordingReplicationNetwork::with_seq(
            repl_sim.clone() as Arc<dyn ReplicationNetwork>,
            clock.clone(),
            capture_seq,
        );
        let repl_addrs: HashMap<String, SocketAddr> = worker_ordinals
            .iter()
            .enumerate()
            .map(|(i, ord)| ((*ord).to_string(), repl_addr_for(i)))
            .collect();
        Self {
            clock,
            name: name.to_string(),
            report_on_drop: std::cell::Cell::new(true),
            repl_recording,
            repl_sim,
            repl_addrs,
            worker_sip_addrs: HashMap::new(),
            all_worker_sip_addrs: Vec::new(),
            markers: Vec::new(),
            event_seq,
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
        self.spawn_proxy_inner(addr, workers, None).await
    }

    /// Like [`spawn_proxy`](Self::spawn_proxy) but with the REAL OPTIONS health
    /// probe running on `probe_addr` (ADR-0012). Health is then driven by actual
    /// probe replies from the workers — `200`→`Alive`, `503` with Reason
    /// `not-ready`→`NotReady` — NOT by `set_health`. This makes the harness
    /// faithful to the production active/passive LB proxy: a freshly-rebooted
    /// worker is observed `Unknown`→`NotReady`→`Alive` as its replication drains,
    /// so the reclaimed call's first keepalive-200 round-trips while the worker is
    /// genuinely non-`Alive` — exercising the response-path reverse-failover guard
    /// (`core/response.rs`) the code ties to the long-call-on-reboot teardown.
    pub async fn spawn_proxy_with_health_probe(
        &self,
        addr: &str,
        probe_addr: &str,
        workers: &[(&str, SocketAddr)],
    ) -> ProxySut {
        self.spawn_proxy_inner(addr, workers, Some(probe_addr)).await
    }

    async fn spawn_proxy_inner(
        &self,
        addr: &str,
        workers: &[(&str, SocketAddr)],
        probe_addr: Option<&str>,
    ) -> ProxySut {
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
            observer.clone(),
            Arc::new(ProxyMetrics::new()),
            self.clock.clone(),
            LoadBalancerConfig::default(),
        ));

        let (ep, sock) = self.harness.bind_sut("proxy", addr).await;
        let metrics = Arc::new(ProxyMetrics::new());
        let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry_dyn.clone())
            .clock(self.clock.clone())
            .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
            .metrics(metrics.clone())
            .build(ep);
        let task = tokio::spawn(core.run());

        // Optional REAL health probe: its own bound endpoint on the fabric, the
        // registry's control seam. Cadence is 10 s / 1.5 s — NOT the production
        // 1 s tick. Every tick fans a real sip-txn OPTIONS to every worker, and
        // under the paused clock that churn is pure CPU (each probe = client txn
        // + worker server txn + Timer E/F/J entries + recorded-trace events the
        // Drop-time RFC audit must scan): at the production cadence the long
        // keepalive cell (≈700 sim-seconds pumped in 100 ms chunks) ran ~420 s
        // wall, ~10 s at this cadence (super-linear: trace-scanning costs grow
        // with probe-event count). Nothing under test depends on the tick period
        // — cells pump until a health transition is observed (`reboot_and_reclaim` waits for
        // Alive, kill/drain set health directly) — so a slower tick is
        // semantics-preserving. Keep both values multiples of the 100 ms advance
        // chunk so there is no paused-clock reply race.
        let probe_task = if let Some(paddr) = probe_addr {
            let (probe_ep, _psock) = self.harness.bind_sut("proxy-probe", paddr).await;
            let control = registry.control();
            let probe = HealthProbe::new(
                probe_ep,
                registry_dyn,
                control,
                observer,
                self.clock.clone(),
                Arc::new(IdGen::seeded(0x9809BE)),
                HealthProbeConfig { interval_ms: 10_000, timeout_ms: 1_500, threshold: 2 },
            );
            Some(tokio::spawn(probe.run()))
        } else {
            None
        };

        ProxySut {
            addr: sock,
            registry,
            metrics,
            task,
            probe_task,
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
        let decision: Arc<dyn CallDecisionEngine> =
            Arc::new(ScriptedDecisionEngine::route_all_to(dest.0, dest.1));
        self.spawn_worker_inner(
            ordinal,
            sip_name,
            sip_bind,
            peers,
            dest,
            outbound_proxy,
            decision,
            Arc::new(NoopLimiter),
        )
        .await
    }

    /// Like [`spawn_worker`](Self::spawn_worker) but with a custom decision
    /// engine (e.g. one returning `call_limiter` entries) and call limiter (e.g.
    /// an `HttpCallLimiter` over a shared HTTP fabric). The limiter survives
    /// crash/reboot (it lives outside the worker), so a failed-over call's holds
    /// are released on the takeover node.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_worker_limited(
        &mut self,
        ordinal: &str,
        sip_name: &str,
        sip_bind: &str,
        peers: &[&str],
        dest: (&str, u16),
        outbound_proxy: (&str, u16),
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
    ) -> ReplicatedB2buaSut {
        self.spawn_worker_inner(
            ordinal, sip_name, sip_bind, peers, dest, outbound_proxy, decision, limiter,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_worker_inner(
        &mut self,
        ordinal: &str,
        sip_name: &str,
        sip_bind: &str,
        peers: &[&str],
        dest: (&str, u16),
        outbound_proxy: (&str, u16),
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
    ) -> ReplicatedB2buaSut {
        let listen_addr = *self
            .repl_addrs
            .get(ordinal)
            .unwrap_or_else(|| panic!("worker {ordinal} was not declared in FailoverHarness::new"));
        let sip_addr: SocketAddr = sip_bind.parse().expect("sip addr");
        self.worker_sip_addrs.insert(ordinal.to_string(), sip_addr);
        self.all_worker_sip_addrs.push(sip_addr);

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
            sip_base_addr: sip_addr,
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
            // Placeholder; replaced by the real handle setup() builds, just below.
            membership: Arc::new(SimulatedMembership::with_clock(vec![], self.clock.clone())),
            harness: self.harness.clone(),
            decision,
            limiter,
        };
        let (setup, store, membership) = sut.wiring.setup(1, &self.clock);
        sut.store = store;
        sut.membership = membership;
        let core = sut.spawn_core(Some(setup)).await;
        sut.metrics = core.metrics().clone();
        sut.core = Some(core);
        sut
    }

    // -- markers / fabric controls ----------------------------------------

    /// Inject a timeline marker stamped with the current clock AND the next
    /// global recording-order sequence — so this lifecycle/chaos transition
    /// interleaves with SIP messages and repl frames in TRUE append order. Called
    /// at the instant the transition occurs (crash/reboot/drain/failover/
    /// partition/heal/cut) in the runner, so e.g. the reboot marker naturally
    /// precedes the bootstrap pull it triggers (Issue 1).
    pub fn mark(&mut self, node: &str, peer: Option<&str>, kind: &str, detail: &str) {
        self.markers.push(Marker {
            at_ms: self.clock.now_ms(),
            seq: self.event_seq.next(),
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
        sip_clock::testkit::pump(dur).await;
    }

    /// **Fine-grained pump toward an unknown timer deadline.** Advances the
    /// paused clock in `step` increments (each a full settle/advance/settle pump
    /// across both planes), running the async `ready` probe *after every step*
    /// and returning `true` the instant it is satisfied — `false` if `max` total
    /// elapses first.
    ///
    /// Use this instead of a fixed [`advance`](Self::advance) whenever the
    /// deadline you need to react at is **not computable in advance** — e.g. a
    /// keepalive the reclaim re-armed a fresh interval out from an unknown
    /// reclaim instant. A fixed advance there either undershoots (the message has
    /// not been emitted yet, so a `receive` would block/auto-advance) or
    /// overshoots (it sails past the deadline *and* the deadline's own reap, e.g.
    /// the 5 s dead-peer timeout, tearing the call down before the test can
    /// answer — the CLAUDE.md keepalive hazard). The pump lets the test stop the
    /// instant the awaited message is queued and answer it inside its window. Pick
    /// `step` smaller than the tightest reaction window (e.g. ≤ 2 s for the 5 s
    /// reap). `ready` is run once *before* the first advance so an already-pending
    /// message costs no extra time; it typically drains the UA endpoints with
    /// [`Agent::try_receive_tolerating`](scenario_harness::Agent::try_receive_tolerating).
    pub async fn pump_until(
        &self,
        step: Duration,
        max: Duration,
        mut ready: impl AsyncFnMut() -> bool,
    ) -> bool {
        if ready().await {
            return true;
        }
        let mut elapsed = Duration::ZERO;
        while elapsed < max {
            self.advance(step).await;
            elapsed += step;
            if ready().await {
                return true;
            }
        }
        false
    }

    /// **Mutualised long-wait teardown settle** (TODO `FixCallTerminateOnBackup`
    /// §5.4). After a call's terminal request, pump the paused clock under the
    /// settle/advance/settle discipline until `drained` is satisfied — first in
    /// fine 200 ms steps for the immediate flush (CDR write + soft limiter release
    /// + reverse-delete drain + an acting-backup takeover copy's self-release on
    /// Timer H/J ~32 s), then in coarse 5 s steps **past one full
    /// `keepalive_interval` (300 s) + `keepalive_timeout` (45 s)** so that any
    /// zombie a buggy teardown left resurrectable *would* have armed its keepalive,
    /// probed, and surfaced (or self-healed) before we assert. Returns `true` if
    /// `drained` was satisfied within the budget, `false` if it timed out (the
    /// caller's `assert_call_fully_over` then reports precisely which invariant is
    /// still broken). Replaces the ad-hoc `for _ in 0..40 { advance(200ms) }` loops
    /// in `limiter_ha.rs` and `runner.rs`.
    pub async fn settle_terminal(&self, mut drained: impl AsyncFnMut() -> bool) -> bool {
        if drained().await {
            return true;
        }
        // Phase 1: fine steps for the fast path (flush + soft release, ~tens of s).
        let mut elapsed = Duration::ZERO;
        let fine = Duration::from_millis(200);
        while elapsed < Duration::from_secs(60) {
            self.advance(fine).await;
            elapsed += fine;
            if drained().await {
                return true;
            }
        }
        // Phase 2: coarse steps past keepalive_interval (300 s) + keepalive_timeout
        // (45 s) + margin, so a resurrected/stranded copy surfaces or self-heals.
        let coarse = Duration::from_secs(5);
        while elapsed < Duration::from_secs(400) {
            self.advance(coarse).await;
            elapsed += coarse;
            if drained().await {
                return true;
            }
        }
        false
    }

    /// Like [`settle_terminal`](Self::settle_terminal) but pumps past the deferral's
    /// replica TTL (`reboot_budget`, 600 s) so the Model-Y backup auto-cleanup reap
    /// has fired: a never-reclaimed deferred terminal has its limiter hold released
    /// and its body evicted only once that TTL expires. The StayDead cells need this
    /// because NOTHING drains until then. Coarse 30 s steps (the reap cadence is
    /// 30 s) to `reboot_budget` + margin; stops as soon as `drained` holds.
    pub async fn settle_lossy_cleanup(&self, mut drained: impl AsyncFnMut() -> bool) -> bool {
        let step = Duration::from_secs(30);
        let mut elapsed = Duration::ZERO;
        while elapsed < Duration::from_secs(720) {
            if drained().await {
                return true;
            }
            self.advance(step).await;
            elapsed += step;
        }
        drained().await
    }

    /// **Post-terminal peer linger** — keep the named peer UAs' sockets open and
    /// *reading* for `window` of virtual time after the scenario's logical end, so
    /// any SIP still in flight at teardown is delivered AND consumed instead of
    /// dropped into an about-to-be-dropped endpoint (reported as "lost in transit")
    /// or left unread in the queue (a `queueLeak` at bind close).
    ///
    /// Two things go wrong without it, both because the synchronous test `drop`
    /// stops the world the instant the cell's body returns:
    /// 1. A datagram a peer *sent* just before the end (a redundant in-dialog BYE
    ///    the owner must answer `481`) never completes its transit hop — nothing
    ///    pumps the paused clock, so the delivery task's `sleep` never fires.
    /// 2. A datagram *delivered to* a peer's queue that the scenario never
    ///    explicitly `receive`d (a relayed final response, a retransmit toward a
    ///    deliberately-silent peer) is dropped unread when the endpoint closes.
    ///
    /// `linger_peers` pumps the clock in fine steps across `window` and drains each
    /// peer after every step (and once up front), modelling a real always-on UA
    /// that keeps its socket open and reading after the call. It asserts nothing —
    /// a cell that wants to *check* a specific late response (e.g. C10's `481` to
    /// bob) still does so explicitly; this only guarantees the trace is free of
    /// teardown-race losses. Call it after [`settle_terminal`](Self::settle_terminal),
    /// before the final invariant assertions.
    pub async fn linger_peers(&self, peers: &[&Agent], window: Duration) {
        let step = Duration::from_millis(200);
        for p in peers {
            p.drain().await;
        }
        let mut elapsed = Duration::ZERO;
        while elapsed < window {
            self.advance(step).await;
            elapsed += step;
            for p in peers {
                p.drain().await;
            }
        }
    }

    // -- report ------------------------------------------------------------

    /// Run the built-in RFC 3261 signaling audit (CSeq in-dialog ordering, …)
    /// over the recorded SIP trace and panic on any violation — the SIP-plane
    /// analogue of the universal teardown sweep's "all clean" check, applied to
    /// EVERY cell once the scenario has fully run. Reads the recording channel
    /// directly (no layer-close structural checks, no consume), so it can run
    /// mid-life on the long-lived multi-SUT harness. Catches a takeover that
    /// probes a dialog with a stale CSeq — a regression a real UAC rejects as
    /// `unexpected_msg` but a test UA answers silently.
    pub fn assert_sip_rfc_clean(&self, cell: &str) {
        let findings = self.rfc_audit_findings();
        assert!(
            findings.is_empty(),
            "[{cell}] SIP RFC audit violation(s) on the recorded trace \
             (a real UA would have rejected these):\n{}",
            findings
                .iter()
                .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    /// The raw RFC 3261 cross-message audit findings over the recorded SIP trace
    /// (the `(lane, detail)` pairs `assert_sip_rfc_clean` panics on). Reads the
    /// recording channel snapshot NON-consuming, so it is safe to call mid-run AND
    /// from `Drop`. Empty ⇒ clean. Shared by the explicit `assert_sip_rfc_clean`
    /// and the automatic Drop-time enforcement so the SAME rule set runs on every
    /// FailoverHarness-based test with no per-test opt-in.
    fn rfc_audit_findings(&self) -> Vec<(String, String)> {
        // RFC 3261 conformance is only observable from OUTSIDE the proxy/LB — at
        // the real UAs (alice/bob). A *transparent* failover legitimately splits
        // ONE logical dialog across several cluster workers: alice's in-dialog
        // CSeq stream 1→2→3 can land 1 on the primary, 2 on the backup (takeover),
        // 3 on the reclaimed primary. Auditing per **worker** bind then reports a
        // phantom "CSeq 3 skips ahead of CSeq 1" on the worker that happened to
        // serve 1 and 3 — even though alice SENT 1,2,3 contiguously and bob
        // RECEIVED 1,2,3 contiguously on its leg. No real UA ever sees that gap.
        //
        // So scope the audit to the dialog as observed at the endpoints, not at
        // the internal cluster nodes: drop the worker binds. Equivalent to
        // regrouping per dialog independent of which worker handled each request —
        // the surviving streams (alice, bob, and the proxy, which forwards EVERY
        // request and so sees the whole 1,2,3 sequence) are each CSeq-monotonic.
        // The endpoint/proxy binds still catch a genuine same-leg CSeq collision
        // (e.g. a keepalive OPTIONS and a later BYE reusing one CSeq toward bob).
        let worker_binds: std::collections::HashSet<String> =
            self.all_worker_sip_addrs.iter().map(|a| a.to_string()).collect();
        let snapshot = self.harness.recording().channel().snapshot();
        let events: Vec<_> = snapshot
            .into_iter()
            .filter(|s| !worker_binds.contains(s.event.bind_key()))
            .collect();
        let mut findings = Vec::new();
        for rule in sip_net::rfc_cross_message_rules() {
            // Honour the advisory tier exactly as the scenario-harness hard gate
            // does: a `force_advisory` rule (a documented B2BUA-architectural
            // divergence — per-leg SDP re-origin, OPTIONS-keepalive response
            // headers, the un-timeable proxy-100 bound, …) is recorded, not
            // gated. Skipping it here keeps the failover matrix from failing on
            // the same architectural divergences the main gate already excuses.
            if rule.force_advisory() {
                continue;
            }
            findings.extend(rule.check(&events));
        }
        findings
    }

    /// Record a rebooted worker's NEW SIP address. Updates the report's
    /// latest-addr map (so the unified report's column tracks the live
    /// incarnation) AND accumulates it into the CSeq-audit exclusion set — the
    /// PRE-reboot incarnation's bind must stay excluded too, else the
    /// endpoint-scoped audit would mistake the worker's internal per-leg CSeq
    /// stream (split across incarnations) for an endpoint skip.
    pub(crate) fn note_worker_rebound(&mut self, ordinal: &str, new_addr: SocketAddr) {
        self.worker_sip_addrs.insert(ordinal.to_string(), new_addr);
        self.all_worker_sip_addrs.push(new_addr);
    }

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

    /// The worker axes (ordinal ↔ SIP addr ↔ repl addr) for the unified report's
    /// combiner, in declaration order so the columns read `b1, b2`.
    fn worker_axes(&self) -> Vec<crate::combine::WorkerAxis> {
        // Declaration order is the repl-addr port order (9400, 9401, …).
        let mut by_ord: Vec<(&String, &SocketAddr)> = self.repl_addrs.iter().collect();
        by_ord.sort_by_key(|(_, addr)| addr.port());
        by_ord
            .into_iter()
            .filter_map(|(ord, repl_addr)| {
                self.worker_sip_addrs.get(ord).map(|sip_addr| crate::combine::WorkerAxis {
                    ordinal: ord.clone(),
                    sip_addr: *sip_addr,
                    repl_addr: *repl_addr,
                })
            })
            .collect()
    }

    /// Build the ONE unified [`seq_report::SeqDoc`] for this run — the SIP plane,
    /// the lifecycle markers, and the replication frames interleaved on the
    /// shared `alice, proxy, b1, b2, bob` lane axis (see [`crate::combine`]).
    ///
    /// Reads the recordings NON-consuming (the SIP channel snapshot + the
    /// recorder snapshot + the repl capture), so it can run mid-life on the
    /// long-lived multi-SUT harness without finishing the SIP harness.
    pub fn unified_doc(&self, title: &str, passed: bool) -> seq_report::SeqDoc {
        let recording = self.harness.recording();
        let entries = sip_net::to_sip_entries(&recording.channel().snapshot());
        let scenario = recording.recorder().snapshot();
        let repl = self.repl_report();
        let mut doc = crate::combine::combine_doc(
            title,
            Some(
                "Unified failover timeline: SIP signaling, lifecycle events \
                 (crash/reboot/failover/partition), and replication frames on one \
                 time-ordered axis (alice / proxy / b1 / b2 / bob).",
            ),
            passed,
            &entries,
            &scenario,
            &repl,
            &self.worker_axes(),
        );
        // RFC 3261 status MUST be reflected in the report: a trace that violates
        // the in-dialog CSeq rule can NEVER show PASS. Fold the findings into the
        // doc anomalies and force passed=false so the rendered report.html /
        // global.txt show FAIL and list the violation(s).
        let findings = self.rfc_audit_findings();
        if !findings.is_empty() {
            doc.passed = false;
            for (lane, detail) in findings {
                doc.anomalies.push(seq_report::Anomaly {
                    check: "rfc3261.cseqInDialogOrder".to_string(),
                    detail,
                    lane: Some(lane),
                    endpoint: None,
                    advisory: Some(false),
                });
            }
        }
        doc
    }

    /// Render the unified report (HTML + global.txt) and write it under `dir` as
    /// `<stem>.html` + `<stem>.global.txt`, plus the replication mermaid as
    /// `<stem>.replication.mmd` (kept as an extra eyeball aid). Reads the
    /// recordings NON-consuming — safe to call mid-run. ALWAYS writes (no env
    /// gating); creates `dir` if absent. Returns the written paths.
    pub fn write_unified_report(
        &self,
        dir: &Path,
        stem: &str,
        title: &str,
        passed: bool,
    ) -> std::io::Result<Vec<PathBuf>> {
        // This run has its own artifacts now — don't also write the Drop fallback.
        self.disarm_report_on_drop();
        std::fs::create_dir_all(dir)?;
        let doc = self.unified_doc(title, passed);
        let mut written = Vec::new();

        let html = dir.join(format!("{stem}.html"));
        std::fs::write(&html, seq_report::render_html(&doc))?;
        written.push(html);

        let txt = dir.join(format!("{stem}.global.txt"));
        std::fs::write(&txt, seq_report::render_global_txt(&doc))?;
        written.push(txt);

        let mmd = dir.join(format!("{stem}.replication.mmd"));
        std::fs::write(&mmd, self.repl_report().render_mermaid())?;
        written.push(mmd);

        Ok(written)
    }

    /// Render the COMBINED unified report as the `global.txt` string: the SIP
    /// exchange, the lifecycle markers, AND the replication exchange interleaved
    /// on one time-ordered axis (see [`unified_doc`](Self::unified_doc)).
    /// Consumes the harness (parity with the historic signature; call last). The
    /// non-consuming [`unified_doc`](Self::unified_doc) /
    /// [`write_unified_report`](Self::write_unified_report) are preferred for the
    /// always-write artifacts.
    pub async fn report(self) -> String {
        // Consuming + explicit: the caller drove the report itself, so suppress
        // the Drop fallback (this `self` is about to drop).
        self.disarm_report_on_drop();
        let doc = self.unified_doc("S10b goal-2 simulated failover", true);
        seq_report::render_global_txt(&doc)
    }

    /// Disarm the write-on-Drop fallback (an explicit report path has run / will
    /// run, or a caller — e.g. `run_cell` — writes its own baseline/variant
    /// artifacts). Idempotent.
    pub fn disarm_report_on_drop(&self) {
        self.report_on_drop.set(false);
    }

    /// The fixed `target/seq-reports/` artifact root for the write-on-Drop
    /// fallback. `CARGO_MANIFEST_DIR` points at `<workspace>/crates/failover-harness`;
    /// the workspace `target/` is two levels up. `CARGO_TARGET_DIR` overrides it if
    /// set (e.g. a custom target dir in CI). Mirrors `runner::seq_reports_dir`.
    fn seq_reports_dir() -> PathBuf {
        if let Ok(t) = std::env::var("CARGO_TARGET_DIR") {
            return PathBuf::from(t).join("seq-reports");
        }
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports")
    }

    /// Sanitize a scenario name into a single filesystem path segment: keep
    /// `[A-Za-z0-9._-]`, fold everything else to `-`, collapse runs, and fall
    /// back to `report` if the result is empty.
    fn sanitize_name(name: &str) -> String {
        let mut out = String::with_capacity(name.len());
        let mut last_dash = false;
        for c in name.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                out.push(c);
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        let trimmed = out.trim_matches('-').to_string();
        if trimmed.is_empty() {
            "report".to_string()
        } else {
            trimmed
        }
    }

    /// SYNC, infallible, panic-free best-effort render of the unified report into
    /// `target/seq-reports/<sanitized name>/report.{html,global.txt,replication.mmd}`.
    /// Used only by [`Drop`] for tests that never called an explicit report path —
    /// so even a panicking/failing test drops its callflow. Skips silently if the
    /// inner harness has already been consumed (the `Mutex<Option<Harness>>` is
    /// empty) or on any IO error. Does NOT touch the async SIP `finish()`/`close()`
    /// path: every read here (channel snapshot, recorder snapshot, repl capture)
    /// is synchronous.
    fn write_report_on_drop(&self) {
        // If the harness was consumed (taken out of the inner Mutex), there is
        // nothing to read — skip. `recording()` would panic otherwise.
        if self
            .harness
            .inner
            .lock()
            .map(|g| g.is_none())
            .unwrap_or(true)
        {
            return;
        }
        let dir = Self::seq_reports_dir().join(Self::sanitize_name(&self.name));
        // unified_doc + render + write are all sync; swallow any IO error.
        let _ = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let doc = self.unified_doc(&self.name, true);
            std::fs::write(dir.join("report.html"), seq_report::render_html(&doc))?;
            std::fs::write(
                dir.join("report.global.txt"),
                seq_report::render_global_txt(&doc),
            )?;
            std::fs::write(
                dir.join("report.replication.mmd"),
                self.repl_report().render_mermaid(),
            )?;
            Ok(())
        })();
    }

    /// Render the unified report and write it under `dir` as `failover.html` +
    /// `failover.global.txt` + `failover.replication.mmd`. Returns the written
    /// paths. Consumes the harness.
    pub async fn write_report(self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.write_unified_report(dir, "failover", "S10b goal-2 simulated failover", true)
    }
}

impl Drop for FailoverHarness {
    /// **Write-on-Drop report fallback** (when still armed): a test that never
    /// called an explicit report path still drops the unified
    /// `report.{html,global.txt,replication.mmd}` (the callflow — including on
    /// panic/failure) under `target/seq-reports/<name>/`. Disarmed paths skip.
    /// Infallible + panic-free: errors are swallowed and a render panic is caught
    /// under `catch_unwind` so Drop never double-panics.
    ///
    /// RFC 3261 audit enforcement is a MANDATORY HARD GATE here: after the
    /// best-effort artifact write (so a failing test still drops its callflow under
    /// `target/seq-reports/<name>/`), the recorded trace's RFC CSeq findings are
    /// computed and — if any exist and the test is not already unwinding — Drop
    /// `panic!`s, failing the test. This is automatic (no per-test opt-in), so
    /// EVERY FailoverHarness-based test whose trace violates the in-dialog CSeq
    /// rule fails. The `!std::thread::panicking()` guard prevents a double-panic
    /// when the test is already failing (e.g. an explicit `assert_sip_rfc_clean`
    /// fired first, or any other assertion). Skips entirely if the inner harness
    /// was consumed (nothing to read).
    fn drop(&mut self) {
        // Best-effort artifact write FIRST, so even a failing cell leaves its
        // callflow under target/seq-reports/<name>/. Disarmed paths skip. The
        // write itself sets passed=false + lists the RFC findings when the trace
        // violates the rule (see write_report_on_drop / unified_doc).
        if self.report_on_drop.get() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.write_report_on_drop()
            }));
        }

        // If the harness was consumed there is nothing to audit — skip (mirrors
        // the write-on-Drop guard).
        if self
            .harness
            .inner
            .lock()
            .map(|g| g.is_none())
            .unwrap_or(true)
        {
            return;
        }

        // Hard gate: a CSeq violation on the recorded trace MUST fail the test.
        // Never double-panic while already unwinding.
        if std::thread::panicking() {
            return;
        }
        let findings = self.rfc_audit_findings();
        if !findings.is_empty() {
            panic!(
                "[{}] SIP RFC 3261 audit violation(s) on the recorded trace — a real \
                 UA would have rejected these, so this test MUST fail (RFC check is a \
                 mandatory hard gate):\n{}",
                self.name,
                findings
                    .iter()
                    .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
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

    /// The recording decorator handle — clones it out under a brief lock so a
    /// caller can read its append-only signaling channel + recorder snapshot
    /// (for the RFC audit AND the unified report) without taking or consuming
    /// the harness.
    fn recording(&self) -> sip_net::RecordingSignalingNetwork {
        let g = self.inner.lock().unwrap();
        g.as_ref().expect("harness taken (already finished?)").recording()
    }
}
