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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use b2bua::cdr::{CdrRecord, InMemoryCdrWriter};
use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::drain::{DrainBounds, DrainExit, DrainOutcome};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{Changelog, ReplicatingCallStore};
use b2bua::store::{CallStore, PartitionRole, PutOpts};
use b2bua::{B2buaCore, ReplicationSetup};
use b2bua_harness::{spawn_proxy_core, B2buaSpawnParams};
use call::{CallBodyCodec, MsgpackCodec, TimerEntry, TimerType};

use ha_harness::{Marker, ReplReport};
use repl_net::transport::{
    Fault, RecordingReplicationNetwork, ReplicationNetwork, SimulatedReplicationNetwork,
};
use scenario_harness::{Agent, Harness};
use sip_clock::Clock;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};
use sip_proxy::{ProxyAddr, ProxyMetrics};
use sip_txn::IdGen;
use tokio::task::JoinHandle;
use topology::{Peer, SimulatedMembership};

use crate::rfc_acceptance::{lane_details, Finding, RfcAcceptance};
use crate::views::{Belief, Incarnation, ViewLedger};

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
    /// The cluster's LIVE `ordinal → repl addr` map, shared by every node: the
    /// resolver reads it per connect attempt (ADR-0012 D3), so re-pointing an
    /// ordinal at a replacement's listen address reaches the running peers.
    addr_map: Arc<Mutex<HashMap<String, SocketAddr>>>,
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
    /// Scenario config mutator (the failover twin of `B2buaSutBuilder::tune`),
    /// applied LAST — after the harness parity defaults — on EVERY core spawn
    /// of this node (initial spawn AND each reboot), so a tuned knob survives
    /// crash/reboot cycles. Default no-op.
    tune: Arc<dyn Fn(&mut b2bua::B2buaConfig) + Send + Sync>,
    /// The cluster's views ledger, shared with the harness: this node registers
    /// each incarnation it spawns and records its own lifecycle beliefs here.
    views: Arc<ViewLedger>,
    /// The LIVE incarnation's presence flag — cleared by `crash`/`reboot` so the
    /// ledger's sampler sees the process go, and the parked handles of a dead
    /// incarnation stop being read as a running node.
    alive: Arc<AtomicBool>,
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
        Self { inner: std::sync::Mutex::new(Some(harness)) }
    }

    /// Bind a SUT endpoint on the shared fabric (under a brief lock). The future
    /// is awaited *after* the lock is released so the guard never crosses the
    /// `.await`.
    async fn bind_sut(
        &self,
        name: &str,
        addr: &str,
    ) -> (Box<dyn sip_net::UdpEndpoint>, SocketAddr) {
        // `bind_udp` is async; the harness is `!Sync`, so we cannot hold the
        // std Mutex guard across the await. `Harness::bind_sut` only registers a
        // lane (sync) + binds — but to keep the guard off the await boundary we
        // take the harness out, bind, then put it back.
        let h = self.inner.lock().unwrap().take().expect("harness taken (already finished?)");
        let res = h.bind_sut(name, addr).await;
        *self.inner.lock().unwrap() = Some(h);
        res
    }

    /// [`bind_sut`](Self::bind_sut) with explicit RFC-audit roles (the
    /// dual-face proxy lanes are `{Proxy}`-only so per-UA subject rules do not
    /// judge a face that carries half a relay's stream).
    async fn bind_sut_with_roles(
        &self,
        name: &str,
        addr: &str,
        roles: std::collections::HashSet<sip_net::UaRole>,
    ) -> (Box<dyn sip_net::UdpEndpoint>, SocketAddr) {
        let h = self.inner.lock().unwrap().take().expect("harness taken (already finished?)");
        let res = h.bind_sut_with_roles(name, addr, roles).await;
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

    /// The SIP wire address this incarnation is bound on (a reboot or a
    /// replacement moves it — a new pod IP).
    pub fn sip_addr(&self) -> SocketAddr {
        self.sip_addr
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
        self.core.as_ref().map(|c| c.metrics()).unwrap_or(&self.metrics)
    }

    /// Non-2xx INVITE finals this worker's transaction layer re-sent on Timer G
    /// (RFC 3261 §17.2.1) — the a-leg server transaction speaking. 0 while crashed.
    pub fn server_final_retransmits(&self) -> u64 {
        self.core.as_ref().map(|c| c.txn_metrics().server_final_retransmits()).unwrap_or(0)
    }

    /// Readiness gate (every reachable peer bootstrapped AND current). Drives the
    /// proxy registry health deterministically (vs. the OPTIONS probe loop).
    pub fn is_ready(&self) -> bool {
        self.core.as_ref().map(|c| c.is_ready()).unwrap_or(false)
    }

    /// The drain latch, whichever set it: SIGTERM or the worker's own observed
    /// withdrawal (ADR-0031 D6). `false` while crashed — a dead incarnation
    /// holds no latch; the views ledger keeps what it believed before.
    pub fn is_draining(&self) -> bool {
        self.core.as_ref().map(|c| c.readiness().is_draining()).unwrap_or(false)
    }

    /// Whether this worker has observed its own endpoint withdrawn from routing
    /// (ADR-0031 D6). `false` while crashed, as [`is_draining`](Self::is_draining).
    pub fn is_withdrawn(&self) -> bool {
        self.core.as_ref().map(|c| c.is_withdrawn()).unwrap_or(false)
    }

    /// This node's replication link toward `peer` as its supervisor holds it
    /// (`Absent` while crashed or unreplicated).
    pub fn peer_link(&self, peer: &str) -> b2bua::repl::PeerLink {
        self.core
            .as_ref()
            .and_then(|c| c.supervisor())
            .map(|s| s.peer_link(peer))
            .unwrap_or(b2bua::repl::PeerLink::Absent)
    }

    /// The retained watermark of the **Backup** flow this node pulls from
    /// `peer` — the stream that carries the peer's forward flushes into this
    /// node's backup partition. Continuous across a condition flip (no puller
    /// respawn); `(0,0)` while crashed or unreplicated.
    pub fn backup_flow_watermark(&self, peer: &str) -> repl_net::frame::Watermark {
        self.core
            .as_ref()
            .and_then(|c| c.supervisor())
            .map(|s| s.flow_watermark(peer, repl_net::frame::Partition::Bak))
            .unwrap_or_else(|| repl_net::frame::Watermark::new(0, 0))
    }

    /// Whether `peer`'s flow on `partition` is connected to THIS node's
    /// replication server and has reported applying everything this node ever
    /// logged for it (ADR-0031 D2) — the per-flow predicate the drain's
    /// caught-up exit is built from. `false` while crashed or unreplicated.
    pub fn flow_caught_up(&self, peer: &str, partition: repl_net::frame::Partition) -> bool {
        self.core
            .as_ref()
            .and_then(|c| c.repl_store())
            .is_some_and(|s| s.changelog().flow_caught_up(peer, partition))
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
    pub async fn get(&self, role: PartitionRole, primary: &str, call_ref: &str) -> Option<Vec<u8>> {
        self.store.get_call(role, primary, call_ref).await.expect("get").map(|b| b.to_vec())
    }

    /// The primary version counter (`p`) currently stored for a ref, or `None`
    /// — projected from the `(p,b)` version vector ([`current_cv`]).
    pub fn call_gen(&self, role: PartitionRole, primary: &str, call_ref: &str) -> Option<i64> {
        self.store.current_cv(role, primary, call_ref).map(|(p, _)| p)
    }

    /// The backup version counter (`b`) currently stored for a ref, or `None`
    /// — the other half of the `(p,b)` vector. `b > 0` means an acting backup
    /// has authored a version of this Element.
    pub fn call_bgen(&self, role: PartitionRole, primary: &str, call_ref: &str) -> Option<i64> {
        self.store.current_cv(role, primary, call_ref).map(|(_, b)| b)
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

    /// The live copy this worker serves for `call_ref`, if any — what its rules
    /// read at the next event.
    pub fn live_call(&self, call_ref: &str) -> Option<call::Call> {
        self.core.as_ref().and_then(|c| c.live_call(call_ref))
    }

    /// HARNESS SURGERY (see `B2buaCore::drop_live_copy`): drop the live
    /// in-memory copy of `call_ref` with NO store mutation — the deterministic
    /// recreation of the rebooted-primary "imported into `pri:{self}` but not
    /// yet materialised" mid-reclaim state, which the bulk-`ReclaimAll` race
    /// only yields under timing.
    pub fn drop_live_copy(&self, call_ref: &str) -> bool {
        self.core.as_ref().map(|c| c.drop_live_copy(call_ref)).unwrap_or(false)
    }

    /// HARNESS SURGERY: implant a stale per-b-leg `NoAnswer` entry into the
    /// replica body this node holds for `call_ref` in `(role, primary)`, firing
    /// at absolute `fire_at_ms` — the stale-guard shape: an entry whose
    /// cancel died with the crashed primary, so the body a later bootstrap +
    /// reclaim serves still carries it. The stored `(p,b)` version is kept, so
    /// pull/reclaim replay the mutated body exactly as they would the original.
    /// Returns the b-leg id the entry names.
    pub async fn implant_stale_no_answer(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        fire_at_ms: i64,
    ) -> String {
        let body = self
            .store
            .get_call(role, primary, call_ref)
            .await
            .expect("replica store read")
            .expect("replica body present");
        let codec = MsgpackCodec::new();
        let mut call = codec.decode(&body).expect("replica body decodes");
        let leg = call.b_legs.first().expect("replicated call has a b-leg").leg_id.clone();
        call.timers.push(TimerEntry {
            id: format!("{:?}:{leg}", TimerType::NoAnswer),
            timer_type: TimerType::NoAnswer,
            fire_at: fire_at_ms,
            leg_id: Some(leg.clone()),
        });
        let (p, b) =
            self.store.current_cv(role, primary, call_ref).expect("replica version vector present");
        self.store
            .put_call(
                role,
                primary,
                call_ref,
                codec.encode(&call),
                &[],
                600_000,
                p,
                b,
                &PutOpts::default(),
            )
            .await
            .expect("replica store write");
        leg
    }

    /// HARNESS SURGERY: rewind the replica body this node holds for `call_ref`
    /// in `(role, primary)` to `body` — an earlier body it genuinely held (read
    /// back with [`get`](Self::get)) — as if every version after it died with
    /// the crashed primary before the peer pulled it. The stored `(p,b)` is
    /// kept, so bootstrap and reclaim replay the rewound body exactly as they
    /// would the latest one.
    pub async fn rewind_replica(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
    ) {
        let (p, b) =
            self.store.current_cv(role, primary, call_ref).expect("replica version vector present");
        self.store
            .put_call(role, primary, call_ref, body, &[], 600_000, p, b, &PutOpts::default())
            .await
            .expect("replica store write");
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
        self.retire_incarnation("crash");
    }

    /// The ledger key of this node's live incarnation (`b1#g2`).
    pub fn incarnation_key(&self) -> String {
        format!("{}#g{}", self.ordinal, self.gen)
    }

    /// Mark the live incarnation's process gone and record it on the ledger.
    fn retire_incarnation(&self, signal: &str) {
        self.alive.store(false, Ordering::SeqCst);
        self.views.record(
            &self.incarnation_key(),
            &self.ordinal,
            Belief::Dead { gen: self.gen },
            signal,
        );
    }

    /// Register the incarnation this node just spawned with the views ledger:
    /// its identity plus the live handles (supervisor peer links, drain latch,
    /// membership) the sampler and the cluster primitives read.
    fn register_incarnation(&self, core: &B2buaCore) {
        self.views.register(Incarnation {
            ordinal: self.ordinal.clone(),
            gen: self.gen,
            sip_addr: self.sip_addr,
            repl_addr: self.wiring.listen_addr,
            alive: self.alive.clone(),
            supervisor: core.supervisor().cloned(),
            readiness: core.readiness(),
            membership: self.membership.clone(),
        });
    }

    /// Begin a graceful drain of this incarnation ([`B2buaCore::drain`]): latch
    /// `Draining` — the proxy's OPTIONS probe then steers new calls away — and
    /// wait for the first of the live calls clearing, a withdrawn worker's
    /// backups holding them past the floor, or the grace. Returns the named
    /// exit + residual. The process keeps serving throughout: draining is not
    /// death.
    pub async fn begin_drain(&self, bounds: DrainBounds) -> DrainOutcome {
        self.views.record(
            &self.incarnation_key(),
            &self.ordinal,
            Belief::Draining { gen: self.gen },
            "drain",
        );
        match self.core.as_ref() {
            Some(core) => core.drain(bounds).await,
            None => {
                DrainOutcome { exit: DrainExit::Quiescent, residual: 0, elapsed: Duration::ZERO }
            }
        }
    }

    /// [`begin_drain`](Self::begin_drain) without the wait: latch `Draining`
    /// now and hand the wait back as a [`PendingDrain`] the test polls while it
    /// drives the timeline. The wait holds only the core's owned probes, so the
    /// process can still be killed mid-drain.
    pub fn begin_drain_detached(&self, bounds: DrainBounds) -> PendingDrain {
        self.views.record(
            &self.incarnation_key(),
            &self.ordinal,
            Belief::Draining { gen: self.gen },
            "drain",
        );
        let probe = self.core.as_ref().map(|core| {
            core.begin_draining();
            core.drain_probe()
        });
        let metrics = self.core.as_ref().map(|core| core.metrics().clone());
        let mut pending = PendingDrain {
            fut: Box::pin(async move {
                match probe {
                    Some(p) => {
                        let out = b2bua::drain::drain_until_quiescent(p, bounds).await;
                        // The detached wait stands in for `B2buaCore::drain`, so
                        // it records the same exit reason the runner would.
                        if let Some(m) = metrics {
                            m.record_drain_exit(out.exit.label(), out.elapsed);
                        }
                        out
                    }
                    None => DrainOutcome {
                        exit: DrainExit::Quiescent,
                        residual: 0,
                        elapsed: Duration::ZERO,
                    },
                }
            }),
            outcome: None,
        };
        // One poll arms the first poll-interval sleep on the paused clock.
        pending.poll();
        pending
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
        self.retire_incarnation("reboot");
        self.alive = Arc::new(AtomicBool::new(true));
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
        let core = self.spawn_core(Some(setup)).await;
        self.register_incarnation(&core);
        self.core = Some(core);

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

    /// Id-stream seed for the live incarnation: FNV-1a over `ordinal ‖ gen`. Two
    /// workers mint distinct Call-ID / Via-branch / To-tag streams (they share one
    /// masqueraded sent-by at the peer, where equal branches merge transactions),
    /// a reboot does not replay its prior life's ids, and the seed is a pure
    /// function of `(ordinal, gen)` so every run stays reproducible.
    fn id_seed(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.ordinal.bytes().chain(self.gen.to_le_bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
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
            id_gen: Arc::new(IdGen::seeded(self.id_seed())),
            cdr: self.cdr.clone(),
            // DETERMINISTIC overload signal (ELU pinned to 0). The default `None`
            // rides `OverloadSignal::live`, whose `LiveLoadSampler` reads the REAL
            // tokio `worker_total_busy_duration` over REAL wall-clock — a signal
            // that has no meaning under a `start_paused` deterministic harness and
            // runs HOT during busy cold-start (spawning both workers + proxy +
            // limiter + health-probe, replication bootstrap). It nondeterministically
            // crossed the 0.75 panic-ELU threshold and shed the establish INVITE with
            // a Tier-3 `panic_elu` 503 (the ~1/5 "cold-start-503" flake; worse under
            // parallel/CI load). Inject a `simulated()` sampler left at ELU 0 so the
            // panic-ELU backstop never trips on runtime busyness; a test that WANTS to
            // exercise it drives a known ELU through the control instead.
            overload: Some(b2bua::overload::OverloadSignal::new(Arc::new(
                b2bua::overload::simulated().0,
            ))),
            // The replicating failover path registers no callflow services, so
            // no `ServiceHttpRequest` is ever fired here.
            adaptation_http: None,
            // Default composition (every built-in CORE machine, incl. the
            // `refer_transfer` seed) — the failover harness does not opt out.
            compose: b2bua::rules::ComposeOptions::default(),
            // Default store + no injected store faults (ADR-0023): the HA
            // stack's behaviour is identical to the pre-seam wiring.
            store: None,
            store_faults: None,
            wire_faults: None,
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
            // The scenario tune runs LAST so it can override any parity
            // default above — on this spawn and on every reboot re-spawn.
            (self.tune)(config);
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
    /// Dual-face mode: the external (caller-plane) face's address. `None` for
    /// the classic single-face proxy.
    ext_addr: Option<SocketAddr>,
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
    /// The proxy's listen address (alice/bob send through it). In dual-face
    /// mode this is the INTERNAL face (the workers' outbound proxy target).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Dual-face mode: the EXTERNAL face's address — what callers dial
    /// `through(..)`. Panics on a single-face proxy (a test asking for the
    /// external face of a single-face proxy is a topology bug).
    pub fn ext_addr(&self) -> SocketAddr {
        self.ext_addr.expect("ext_addr on a single-face proxy — spawn with spawn_proxy_dual")
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
        self.registry.set_address(ordinal, ProxyAddr::new(addr.ip().to_string(), addr.port()));
    }

    /// Drop `ordinal` from the proxy's MEMBERSHIP — the endpoint is gone from
    /// its view entirely (not a health annotation: the ordinal no longer
    /// resolves, so nothing routes to it and no cookie can name it). The
    /// worker's process is untouched.
    pub fn remove_worker(&self, ordinal: &str) {
        self.registry.remove(ordinal);
    }

    /// Publish `ordinal` at `addr` in the proxy's membership, health `Unknown`
    /// — a freshly observed endpoint the probe has not judged yet. Use
    /// [`set_health`](Self::set_health) to state a judged health.
    pub fn add_worker(&self, ordinal: &str, addr: SocketAddr) {
        self.registry.add(sip_proxy::registry::WorkerEntry {
            id: ordinal.to_string(),
            address: ProxyAddr::new(addr.ip().to_string(), addr.port()),
            health: WorkerHealth::Unknown,
            draining_since: None,
            first_seen_at_ms: None,
        });
    }

    /// The health the proxy holds for the worker bound at `addr`, an address
    /// that left the set included (`Dead` for Timer H after a departure; `None`
    /// once its window closed or for an address the pool never served).
    pub fn health_at(&self, addr: SocketAddr) -> Option<WorkerHealth> {
        self.registry
            .lookup_by_address(&ProxyAddr::new(addr.ip().to_string(), addr.port()))
            .map(|w| w.health)
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
    /// `ordinal → DECLARED repl addr` (stable across reboots), for fault/lane
    /// mapping. A replacement listens elsewhere — see `repl_resolver`.
    repl_addrs: HashMap<String, SocketAddr>,
    /// The cluster's LIVE `ordinal → repl addr` map every node's resolver reads
    /// per connect attempt (ADR-0012 D3). Initialised from `repl_addrs`;
    /// [`spawn_replacement`](Self::spawn_replacement) re-points an ordinal at
    /// the new incarnation's listen address.
    repl_resolver: Arc<Mutex<HashMap<String, SocketAddr>>>,
    /// EVERY worker SIP addr ever bound, across all incarnations. The
    /// endpoint-scoped RFC CSeq audit excludes worker binds (a transparent
    /// failover splits one dialog's CSeq stream across workers, which the audit
    /// would misread as a skip); after a reboot moves a worker to a new addr its
    /// PRE-reboot bind must stay excluded too, so accumulate rather than replace.
    all_worker_sip_addrs: Vec<SocketAddr>,
    /// Dual-face proxy bind addrs (both faces, every takeover twin). Excluded
    /// from the Drop-time endpoint-scoped audit exactly like worker binds: a
    /// dual-face relay splits one dialog's stream across its two binds
    /// (request in on one face, out on the other), so neither face alone is an
    /// endpoint-shaped stream — conformance is judged at alice/bob. The
    /// single-face proxy bind is NOT here (its one lane sees both directions
    /// and stays audited, unchanged).
    relay_sip_addrs: Vec<SocketAddr>,
    /// Injected timeline markers (crash/reboot/failover/partition/…).
    markers: Vec<Marker>,
    /// The ONE shared global recording-order sequencer (the SIP recorder's
    /// `EventSequencer`). Markers are stamped from it at the instant of
    /// `mark()`/`partition()`/`heal()`/crash/reboot so they interleave with SIP
    /// messages and repl frames in true append order (Issue 1).
    event_seq: Arc<layer_harness::EventSequencer>,
    /// The SIP harness handle (shared so workers can re-bind on reboot). It also
    /// carries this run's log/trace capture: the inner `scenario_harness::Harness`
    /// installs the thread-scoped `observe` buffer and dumps it to stderr when a
    /// failover scenario panics, alongside the wire trace (ADR-0026).
    harness: Arc<HarnessHandle>,
    /// **Per-node wall-clock anchor offset (ms)** for clock-skew hardening tests.
    /// The harness rides ONE monotonic `tokio::time` timeline, but each node's
    /// `Clock` can carry a DIFFERENT wall anchor: a node with offset `+30_000`
    /// reads `now_ms()` 30 s ahead of a node at offset `0`, while behaviour timers
    /// stay on the shared monotonic clock. That is exactly deterministic inter-node
    /// wall skew under a paused runtime — the one thing the single-clock harness
    /// could not previously reproduce (CLAUDE.md "single-clock fidelity gap"). A
    /// node's clock is `Clock::test_at(offset)` (the harness base anchor is 0);
    /// every clock consumer for that node (b2bua core, changelog/store, membership)
    /// uses it, so a replica it flushes stamps `origin_now_ms` in ITS frame and the
    /// receiver computes the true cross-node offset. Default 0 (no skew).
    worker_clock_offsets: HashMap<String, i64>,
    /// This run's declared RFC-audit scoping — lifetime waivers plus acceptance
    /// windows (see [`crate::rfc_acceptance`]). Everything it does not cover
    /// gates.
    rfc_acceptance: RfcAcceptance,
    /// Worker config mutator applied on every node's core spawn AND reboot
    /// (after the parity defaults) — set via
    /// [`with_worker_tune`](Self::with_worker_tune) BEFORE spawning workers.
    worker_tune: Arc<dyn Fn(&mut b2bua::B2buaConfig) + Send + Sync>,
    /// The views ledger: every observer's belief about every worker, written by
    /// the cluster primitives and by the per-chunk sampler in
    /// [`advance`](Self::advance). Shared with each worker SUT.
    views: Arc<ViewLedger>,
    /// Per-ordinal spawn recipe, kept so
    /// [`spawn_replacement`](Self::spawn_replacement) can stand a second
    /// incarnation of an ordinal up with the original's wiring.
    worker_specs: HashMap<String, WorkerSpec>,
    /// SIP addresses cut off the signalling fabric: every send from or to one
    /// of them fails at its endpoint, so the node is unreachable while its
    /// process keeps running and keeps firing its own timers. Read by the
    /// fabric's send-fault hook; written by
    /// [`cut_signalling`](Self::cut_signalling).
    sip_cut: Arc<Mutex<std::collections::BTreeSet<SocketAddr>>>,
}

/// How one worker ordinal was spawned — everything a replacement incarnation of
/// the same ordinal needs.
#[derive(Clone)]
struct WorkerSpec {
    sip_name: String,
    sip_base_addr: SocketAddr,
    peers: Vec<String>,
    dest: (String, u16),
    outbound_proxy: (String, u16),
    decision: Arc<dyn CallDecisionEngine>,
    limiter: Arc<dyn CallLimiter>,
}

/// The in-dialog CSeq-ordering audit rule (`rfc_rules::rules::cseq`), named here so
/// the failover tests can waive it by a symbol rather than a bare string. See
/// [`FailoverHarness::accept_rfc_deviations_from_now`].
pub const RULE_CSEQ_IN_DIALOG_ORDER: &str = "cseq-in-dialog-order";

/// `127.0.0.1:9400+n` — a stable per-ordinal repl listen address.
fn repl_addr_for(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9400 + index as u16))
}

/// A drain the test started and has not waited out: the node is latched
/// `Draining` and keeps serving its live calls while the timeline runs. Owns
/// its wait (an active-call probe, not the node), so the caller stays free to
/// kill the process mid-drain. Created by
/// [`FailoverHarness::begin_drain_pending`].
pub struct PendingDrain {
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = DrainOutcome> + Send>>,
    outcome: Option<DrainOutcome>,
}

impl PendingDrain {
    /// The drain's outcome once it has returned — why it exited, the residual
    /// live-call count and how long it waited — `None` while it still waits.
    /// Re-polls the drain, so call it after every advance: nothing else drives
    /// this future.
    pub fn poll(&mut self) -> Option<DrainOutcome> {
        if self.outcome.is_none() {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(out) = self.fut.as_mut().poll(&mut cx) {
                self.outcome = Some(out);
            }
        }
        self.outcome
    }
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
        let sip_cut: Arc<Mutex<std::collections::BTreeSet<SocketAddr>>> =
            Arc::new(Mutex::new(std::collections::BTreeSet::new()));
        let cut = sip_cut.clone();
        let fault: sip_net::SendFault = Arc::new(move |src: SocketAddr, dst: SocketAddr| {
            let cut = cut.lock().unwrap();
            (cut.contains(&src) || cut.contains(&dst))
                .then(|| format!("{src} is cut off the signalling fabric"))
        });
        let harness = Harness::with_transit_delay_and_send_fault(name, 1, fault).describe(
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
        let event_seq_for_views = event_seq.clone();
        Self {
            clock: clock.clone(),
            name: name.to_string(),
            report_on_drop: std::cell::Cell::new(true),
            repl_recording,
            repl_sim,
            repl_resolver: Arc::new(Mutex::new(repl_addrs.clone())),
            repl_addrs,
            all_worker_sip_addrs: Vec::new(),
            relay_sip_addrs: Vec::new(),
            markers: Vec::new(),
            event_seq,
            harness: Arc::new(HarnessHandle::new(harness)),
            worker_clock_offsets: HashMap::new(),
            rfc_acceptance: RfcAcceptance::default(),
            worker_tune: Arc::new(|_| {}),
            views: ViewLedger::new(clock, event_seq_for_views),
            worker_specs: HashMap::new(),
            sip_cut,
        }
    }

    /// Set the worker config mutator (the failover twin of
    /// `B2buaSutBuilder::tune`): applied to every worker's [`b2bua::B2buaConfig`]
    /// after the harness parity defaults, on the initial spawn AND on every
    /// reboot re-spawn — so a tuned knob (e.g. `invite_txn_timeout_sec`)
    /// survives crash/reboot cycles. Call BEFORE spawning workers; workers
    /// already spawned keep the tune they captured. Returns `self` for chaining.
    pub fn with_worker_tune(
        mut self,
        tune: impl Fn(&mut b2bua::B2buaConfig) + Send + Sync + 'static,
    ) -> Self {
        self.worker_tune = Arc::new(tune);
        self
    }

    /// Waive one RFC-audit rule (by its `name()`) on this harness's Drop-time hard
    /// gate for the **whole harness lifetime** — establishment included, before any
    /// fault exists. It leaves no baseline in this recording: nothing this run
    /// emits is judged by `rule` again, so a NEW regression in the same rule is
    /// invisible here. Use it ONLY for a deviation whose cause spans the entire run
    /// (a peer fixture that is non-compliant by construction); for a deviation that
    /// exists only after a fault is injected use
    /// [`accept_rfc_deviations_from_now`](Self::accept_rfc_deviations_from_now),
    /// which keeps the rule gating everywhere outside the window. Every OTHER rule
    /// gates either way. `justification` documents WHY at the call site.
    pub fn allow_rfc_violation(&mut self, rule: &str, justification: &str) {
        debug_assert!(!justification.trim().is_empty(), "waiver needs a justification");
        let _ = justification;
        self.rfc_acceptance.waive_lifetime(rule);
    }

    /// Accept `rule`'s findings on messages recorded from **now** until
    /// [`resume_rfc_gate`](Self::resume_rfc_gate) (or the end of the run) — the
    /// window-scoped counterpart of [`allow_rfc_violation`]. Call it at the instant
    /// the scenario injects its fault: establishment, every message before the
    /// injection, and every no-fault scenario keep the rule fully gating, so a new
    /// regression outside the window still fails the test.
    ///
    /// The boundary is the recording's **capture order** (`seq`), not a timestamp:
    /// under a paused clock a whole burst shares one `at_ms`, so a message captured
    /// before this call is outside the window even when it carries the same
    /// millisecond.
    ///
    /// A finding the rule cannot pin to a wire entry is unattributable and stays
    /// gated (only [`allow_rfc_violation`] covers it).
    ///
    /// Accepted findings are classified, not masked: they leave the gate but land
    /// in [`accepted_rfc_deviations`](Self::accepted_rfc_deviations) and in the
    /// unified report as advisory anomalies, and the scenario still asserts the
    /// accepted OUTCOME (call fully over, one CDR, limiter released).
    ///
    /// The one deviation this exists for is [`RULE_CSEQ_IN_DIALOG_ORDER`] across a
    /// takeover window: ADR-0014's accepted keepalive-vs-backup-transaction
    /// overlap. Two owners of one leg (a proxy-Dead-but-alive primary and/or a
    /// stale-hydrated backup) each mint `dialog.sip.local_cseq + 1`; `(p,b)` rejects
    /// the loser, but its mutation already reached the wire. The reused number rides
    /// a FRESH `branch`, so a compliant UAS does not fold it away as a
    /// retransmission: it sees a new server transaction whose sequence number did
    /// not advance and rejects it out of order (RFC 3261 §12.2.2 — 500, or an
    /// implementation-defined reject of the stale number). That one call drops
    /// cleanly; nothing else on the dialog is affected.
    pub fn accept_rfc_deviations_from_now(&mut self, rule: &str, justification: &str) {
        debug_assert!(!justification.trim().is_empty(), "acceptance needs a justification");
        let _ = justification;
        let from = self.recorded_seq_high_water();
        self.rfc_acceptance.open_window(rule, from);
    }

    /// Close EVERY open acceptance window for `rule` at this instant — the rule
    /// gates in full again from here on. No-op when no window for `rule` is open.
    pub fn resume_rfc_gate(&mut self, rule: &str) {
        let until = self.recorded_seq_high_water();
        self.rfc_acceptance.close_windows(rule, until);
    }

    /// The highest recording `seq` captured on the SIP channel so far — the
    /// capture-order boundary an acceptance window is anchored on. `0` before the
    /// first recorded event.
    fn recorded_seq_high_water(&self) -> u64 {
        self.harness.recording().channel().snapshot().iter().map(|s| s.seq).max().unwrap_or(0)
    }

    /// Set a worker's **wall-clock anchor offset** (ms) for a clock-skew test —
    /// see [`worker_clock_offsets`](Self::worker_clock_offsets). Call BEFORE
    /// [`spawn_worker`](Self::spawn_worker) for `ordinal` (the offset is read at
    /// spawn and carried across reboots). A positive offset anchors the node AHEAD
    /// of true time (skew-ahead), negative BEHIND. Returns `self` for chaining.
    pub fn with_worker_clock_offset(mut self, ordinal: &str, offset_ms: i64) -> Self {
        self.worker_clock_offsets.insert(ordinal.to_string(), offset_ms);
        self
    }

    /// This node's `Clock` — the shared monotonic timeline with the node's own
    /// wall anchor applied (0 offset ⇒ the harness base clock). Under a paused
    /// runtime with no advance yet at spawn, every node's `anchor_instant` is the
    /// same tokio `Instant`, so `Clock::test_at(offset)` yields `now_ms() = offset
    /// + shared_elapsed` — deterministic inter-node skew on one monotonic clock.
    fn worker_clock(&self, ordinal: &str) -> Clock {
        match self.worker_clock_offsets.get(ordinal).copied() {
            Some(offset) if offset != 0 => Clock::test_at(offset),
            _ => self.clock.clone(),
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

    /// [`agent`](Self::agent) with an arrival-time [`sip_net::PreIngressHook`]
    /// on the UA's bind — the deterministic loss seam (drop a chosen datagram
    /// before it reaches the agent's inbox; see
    /// `scenario_harness::Harness::agent_with_pre_ingress`).
    pub async fn agent_with_pre_ingress(
        &self,
        name: &str,
        addr: &str,
        hook: sip_net::PreIngressHook,
    ) -> Agent {
        self.harness.agent_with_pre_ingress(name, addr, hook).await
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
        // The registry→hmac→observer→strategy→ProxyCore wiring is the shared
        // `b2bua_harness::spawn_proxy_core` primitive (ADR-0013 §0): bind the
        // proxy endpoint here, then hand it the multi-worker slice + this
        // harness's shared clock. The returned parts carry the CONCRETE registry
        // (retained below so `set_health`/`set_address` drive the live proxy) and
        // the SAME load observer the strategy reads (fed by the probe's
        // `X-Overload` payloads below — failover-only).
        let (ep, sock) = self.harness.bind_sut("proxy", addr).await;
        let parts = spawn_proxy_core(ep, sock, workers, self.clock.clone());
        let b2bua_harness::ProxyCoreParts { addr: sock, registry, metrics, observer, task } = parts;

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
            let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
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

        // The registry is the `proxy` observer's source — the ledger samples it
        // for presence ⊕ health after every advance chunk.
        self.views.attach_proxy(registry.clone());
        ProxySut { addr: sock, ext_addr: None, registry, metrics, task, probe_task }
    }

    /// Stand up a **dual-face** load-balancing proxy SUT: the internal face at
    /// `int_addr` (workers' outbound-proxy target, worker plane), the external
    /// face at `ext_addr` (what callers dial), and `int_cidrs` (the
    /// `PROXY_FACE_INT_CIDRS` equivalent) driving the per-destination egress
    /// face picker. `name` prefixes the two bind lanes (`{name}-int` /
    /// `{name}-ext`); `id_seed` seeds the proxy's Via-branch generator — a
    /// takeover twin re-bound on the SAME VIPs must use a different seed so it
    /// cannot replay the dead proxy's branch sequence into live dialogs.
    ///
    /// Both faces are registered as **relay lanes**: the Drop-time
    /// endpoint-scoped RFC audit excludes them (same rationale as worker
    /// binds — a dual-face relay splits one dialog's stream across its two
    /// binds, so neither face alone is an endpoint-shaped stream; conformance
    /// is judged at alice/bob, which see everything the proxy emits). The
    /// lanes are role-tagged `{Proxy}` so the full-suite audit
    /// ([`assert_full_rfc_clean`](Self::assert_full_rfc_clean)) judges them
    /// under the proxy-subject rules only.
    pub async fn spawn_proxy_dual(
        &mut self,
        name: &str,
        int_addr: &str,
        ext_addr: &str,
        int_cidrs: &str,
        workers: &[(&str, SocketAddr)],
        id_seed: u64,
    ) -> ProxySut {
        let roles = std::collections::HashSet::from([sip_net::UaRole::Proxy]);
        let (int_ep, int_sock) =
            self.harness.bind_sut_with_roles(&format!("{name}-int"), int_addr, roles.clone()).await;
        let (ext_ep, ext_sock) =
            self.harness.bind_sut_with_roles(&format!("{name}-ext"), ext_addr, roles).await;
        self.relay_sip_addrs.push(int_sock);
        self.relay_sip_addrs.push(ext_sock);

        let external = b2bua_harness::ExternalProxyFace {
            endpoint: ext_ep,
            addr: ext_sock,
            int_cidrs: sip_proxy::FaceCidrs::parse(int_cidrs)
                .unwrap_or_else(|e| panic!("bad dual-face int CIDRs {int_cidrs:?}: {e}")),
        };
        let parts = b2bua_harness::spawn_proxy_core_with(
            int_ep,
            int_sock,
            Some(external),
            workers,
            self.clock.clone(),
            id_seed,
        );
        let b2bua_harness::ProxyCoreParts { addr: sock, registry, metrics, observer: _, task } =
            parts;
        ProxySut { addr: sock, ext_addr: Some(ext_sock), registry, metrics, task, probe_task: None }
    }

    /// The recorded SIP wire entries so far (every send paired with its
    /// delivery — `from`/`to` are the REAL socket addresses). Non-consuming;
    /// the dual-face tests assert per-face source-address discipline on it
    /// (every datagram a caller receives originates from the external face's
    /// bind, every worker-bound one from the internal face's).
    pub fn sip_entries(&self) -> Vec<sip_net::RecordedSipEntry> {
        sip_net::to_sip_entries(&self.harness.recording().channel().snapshot())
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
            ordinal,
            sip_name,
            sip_bind,
            peers,
            dest,
            outbound_proxy,
            decision,
            limiter,
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
        let spec = WorkerSpec {
            sip_name: sip_name.to_string(),
            sip_base_addr: sip_bind.parse().expect("sip addr"),
            peers: peers.iter().map(|p| (*p).to_string()).collect(),
            dest: (dest.0.to_string(), dest.1),
            outbound_proxy: (outbound_proxy.0.to_string(), outbound_proxy.1),
            decision,
            limiter,
        };
        self.worker_specs.insert(ordinal.to_string(), spec.clone());
        self.spawn_incarnation(ordinal, &spec, 1, spec.sip_base_addr, listen_addr).await
    }

    /// Stand ONE incarnation of `ordinal` up: bind its SIP endpoint at
    /// `sip_addr`, listen for replication at `listen_addr`, and spawn a core at
    /// incarnation `gen` over a fresh empty store. Shared by the first spawn and
    /// by [`spawn_replacement`](Self::spawn_replacement); a reboot re-spawns
    /// in place on the SUT itself.
    async fn spawn_incarnation(
        &mut self,
        ordinal: &str,
        spec: &WorkerSpec,
        gen: u64,
        sip_addr: SocketAddr,
        listen_addr: SocketAddr,
    ) -> ReplicatedB2buaSut {
        self.all_worker_sip_addrs.push(sip_addr);

        // Every node resolves peers through the cluster's ONE live map
        // (ADR-0012 D3), so a later replacement re-points this ordinal without
        // touching the running peers. A first incarnation's list holds the node
        // itself, as the informer's slice shows a published pod: that is how it
        // observes its own withdrawal (ADR-0031 D6); the supervisor never pulls
        // itself. A replacement is published by `readmit`, not at spawn.
        let published = if gen == 1 { Some(ordinal) } else { None };
        let peer_list: Vec<Peer> = spec
            .peers
            .iter()
            .map(String::as_str)
            .chain(published)
            .map(|p| Peer::new(p, p))
            .collect();
        let wiring = ReplWiring {
            network: Arc::new(self.repl_recording.clone()) as Arc<dyn ReplicationNetwork>,
            addr_map: self.repl_resolver.clone(),
            peers: peer_list,
            listen_addr,
            ttls: DEFAULT_TTLS,
        };

        // This node's own wall clock (shared monotonic timeline + per-node anchor
        // offset). Every clock consumer for the node uses it, so a replica it
        // flushes stamps `origin_now_ms` from ITS wall clock and the receiver
        // computes the true cross-node skew offset (clock-skew hardening harness).
        let node_clock = self.worker_clock(ordinal);
        let cdr = InMemoryCdrWriter::new();
        let mut sut = ReplicatedB2buaSut {
            ordinal: ordinal.to_string(),
            sip_addr,
            sip_base_addr: spec.sip_base_addr,
            sip_name: spec.sip_name.clone(),
            sip_bind: sip_addr.to_string(),
            gen,
            cdr: cdr.clone(),
            metrics: B2buaMetrics::new(),
            dest: spec.dest.clone(),
            outbound_proxy: Some(spec.outbound_proxy.clone()),
            wiring,
            clock: node_clock.clone(),
            core: None,
            store: Arc::new(ReplicatingCallStore::new(gen, node_clock.clone())),
            // Placeholder; replaced by the real handle setup() builds, just below.
            membership: Arc::new(SimulatedMembership::with_clock(vec![], node_clock.clone())),
            harness: self.harness.clone(),
            decision: spec.decision.clone(),
            limiter: spec.limiter.clone(),
            tune: self.worker_tune.clone(),
            views: self.views.clone(),
            alive: Arc::new(AtomicBool::new(true)),
        };
        let (setup, store, membership) = sut.wiring.setup(gen, &node_clock);
        sut.store = store;
        sut.membership = membership;
        let core = sut.spawn_core(Some(setup)).await;
        sut.metrics = core.metrics().clone();
        sut.register_incarnation(&core);
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

    /// Partition two workers on the repl fabric: block a fresh `connect`
    /// between their listen addresses AND stop delivery on every stream their
    /// pullers already opened, both directions. A puller connects from an
    /// ephemeral client address, so the listen-pair fault alone never reaches a
    /// live stream — the established directions are named here from the
    /// `PullRequest` each puller opened with. A stalled direction buffers in
    /// order and flushes on [`heal`](Self::heal). Marker.
    pub fn partition(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_addrs[a], self.repl_addrs[b]);
        self.repl_sim.apply_fault(Fault::Partition { a: aa, b: ba });
        for (src, dst) in self.live_stream_pairs(a, b) {
            self.repl_sim.apply_fault(Fault::Stall { src, dst });
        }
        self.mark(a, Some(b), "partition", "");
    }

    /// Every established directed pair between `a` and `b`'s replication
    /// endpoints: each one's listener against the other's puller client
    /// addresses, both ways. A client address is recovered from the `caller`
    /// its opening `PullRequest` names, so a stream is attributed to the node
    /// that opened it rather than guessed from the frame's direction.
    fn live_stream_pairs(&self, a: &str, b: &str) -> Vec<(SocketAddr, SocketAddr)> {
        let report = self.repl_report();
        let clients_of = |ordinal: &str| -> std::collections::BTreeSet<SocketAddr> {
            report
                .frames
                .iter()
                .filter_map(|f| match &f.frame {
                    repl_net::frame::Frame::PullRequest { caller, .. } if caller == ordinal => {
                        Some(f.from)
                    }
                    _ => None,
                })
                .filter(|addr| !self.repl_addrs.values().any(|l| l == addr))
                .collect()
        };
        let mut pairs = Vec::new();
        for (server, client_owner) in [(a, b), (b, a)] {
            let listener = self.repl_addrs[server];
            for client in clients_of(client_owner) {
                pairs.push((listener, client));
                pairs.push((client, listener));
            }
        }
        pairs
    }

    /// Delay delivery on every replication stream `from`'s listener serves by
    /// `ms` — the frames it sends down the connections pullers opened to it
    /// (both of `to`'s flows). A flush `from` writes lands on `to` only `ms`
    /// later. Marker.
    pub fn delay_streams_from(&mut self, from: &str, to: &str, ms: u64) {
        let listener = self.repl_addrs[from];
        let listeners: Vec<SocketAddr> = self.repl_addrs.values().copied().collect();
        let clients: std::collections::BTreeSet<SocketAddr> = self
            .repl_report()
            .frames
            .iter()
            .filter(|f| f.from == listener && !listeners.contains(&f.to))
            .map(|f| f.to)
            .collect();
        assert!(!clients.is_empty(), "no replication stream from {from}'s listener to a puller");
        for client in &clients {
            self.repl_sim.apply_fault(Fault::Delay { src: listener, dst: *client, ms });
        }
        self.mark(from, Some(to), "delay", &format!("{ms}ms on {listener} → {clients:?}"));
    }

    /// **Cut `addr` off the signalling fabric, both directions** — the node is
    /// unreachable on the SIP plane while its process keeps running, keeps its
    /// calls and keeps firing their timers. Every datagram it sends and every
    /// datagram addressed to it fails at the sending endpoint, so nothing
    /// crosses and nothing queues. The replication fabric is separate
    /// ([`partition`](Self::partition)). Marker.
    pub fn cut_signalling(&mut self, ordinal: &str, addr: SocketAddr) {
        self.sip_cut.lock().unwrap().insert(addr);
        self.mark(ordinal, None, "sip-partition", &format!("{addr} unreachable on the SIP plane"));
    }

    /// Restore a signalling cut. Marker.
    pub fn restore_signalling(&mut self, ordinal: &str, addr: SocketAddr) {
        self.sip_cut.lock().unwrap().remove(&addr);
        self.mark(ordinal, None, "sip-heal", &format!("{addr} reachable again"));
    }

    /// Heal a repl-fabric partition: unblock `connect` and resume every stalled
    /// stream, which flushes what buffered while the cut lasted, in order.
    /// Marker.
    pub fn heal(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_addrs[a], self.repl_addrs[b]);
        for (src, dst) in self.live_stream_pairs(a, b) {
            self.repl_sim.apply_fault(Fault::Resume { src, dst });
        }
        self.repl_sim.apply_fault(Fault::Heal { a: aa, b: ba });
        self.mark(a, Some(b), "heal", "");
    }

    // -- membership primitives (the orchestrator's hand) ---------------------

    /// **Withdraw `ordinal`'s endpoint** — the orchestrator takes the worker out
    /// of the cluster's view WITHOUT touching its process: the proxy's registry
    /// drops the ordinal (nothing routes to it, no cookie resolves it) and every
    /// other live worker's membership drops the peer (its supervisor parks the
    /// flows), and so does the worker's own — it observes its withdrawal and
    /// latches Draining (ADR-0031 D6). The process stays bound and keeps serving
    /// whatever still reaches it — the zombie shape. Records the orchestrator's,
    /// the proxy's and every peer's belief at this instant; the sampler then
    /// records what each component actually did with it.
    pub fn withdraw(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "withdraw", "endpoint withdrawn; process still running");
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "withdraw");
        if let Some(registry) = self.views.proxy_registry() {
            registry.remove(ordinal);
            self.views.record("proxy", ordinal, Belief::Absent, "withdraw");
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.remove(ordinal);
            self.views.record(&observer, ordinal, Belief::Absent, "withdraw");
        }
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.remove(ordinal);
        }
    }

    /// **Withdraw `ordinal` from routing, keep it a replication peer** — the
    /// graceful shape (ADR-0031 case 1): the orchestrator has begun to remove the
    /// worker, so its endpoint reads `ready=false, terminating=true` but STAYS in
    /// the slice until the pod is gone. The proxy departs the ordinal (out of the
    /// projection, address tombstoned; no cookie resolves it) while every other
    /// live worker keeps pulling it on presence alone (D1). The process is
    /// untouched and keeps serving whatever still reaches it. Records the
    /// orchestrator's, the proxy's and every peer's belief at this instant.
    pub fn withdraw_routing(&mut self, ordinal: &str) {
        self.mark(
            ordinal,
            None,
            "withdraw_routing",
            "endpoint not ready + terminating, still in the slice; process still running",
        );
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "withdraw_routing");
        self.set_member_ready(ordinal, false, true, "withdraw_routing");
    }

    /// **Flap `ordinal` not ready at the same address** — the restarted-in-place
    /// shape (ADR-0031 case 4): a readiness probe fails, the endpoint reads
    /// `ready=false, terminating=false` and stays in the slice. The proxy departs
    /// the ordinal and tombstones its address; every other live worker keeps
    /// pulling it (D1). The process is untouched.
    pub fn flap_not_ready(&mut self, ordinal: &str) {
        self.mark(
            ordinal,
            None,
            "flap_not_ready",
            "endpoint not ready, same address, in the slice",
        );
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "flap_not_ready");
        self.set_member_ready(ordinal, false, false, "flap_not_ready");
    }

    /// **Flap `ordinal` ready again** — the counterpart of
    /// [`flap_not_ready`](Self::flap_not_ready): the endpoint reads `ready=true`
    /// at the same address. The proxy publishes it as a fresh `Unknown` endpoint
    /// (nothing has probed it yet); every other live worker's link is a plain
    /// active peer again, with no puller respawn (ADR-0031 D1).
    pub fn flap_ready(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "flap_ready", "endpoint ready again at the same address");
        self.views.record("orchestrator", ordinal, Belief::Admitted, "flap_ready");
        self.set_member_ready(ordinal, true, false, "flap_ready");
    }

    /// Drive one endpoint-condition change into the proxy's registry and every
    /// live peer's membership, recording what each observer is expected to hold:
    /// the proxy departs a not-ready member and publishes a ready one `Unknown`;
    /// a peer keeps a not-ready member and treats a ready one as plain active.
    fn set_member_ready(&mut self, ordinal: &str, ready: bool, terminating: bool, signal: &str) {
        if let Some(registry) = self.views.proxy_registry() {
            registry.set_ready(ordinal, ready);
            let belief =
                if ready { Belief::Registered(WorkerHealth::Unknown) } else { Belief::Absent };
            self.views.record("proxy", ordinal, belief, signal);
        }
        let belief = if ready { Belief::PeerActive } else { Belief::PeerKept };
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.set_conditions(ordinal, ready, terminating);
            self.views.record(&observer, ordinal, belief, signal);
        }
        // The informer shows a worker its own endpoint too: a condition flip
        // reaches the member itself, which is how it observes its own
        // withdrawal (ADR-0031 D6).
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.set_conditions(ordinal, ready, terminating);
        }
    }

    /// **Depart `ordinal`: its endpoint leaves the slice** — the pod is gone
    /// (ADR-0031 case 1, the instant after the drain). The proxy drops the
    /// ordinal (it was already unroutable under
    /// [`withdraw_routing`](Self::withdraw_routing)) and every other live
    /// worker's membership drops the peer, so its supervisor parks the flows.
    /// The process, if still running, is untouched: pair with `crash` to model
    /// the exit.
    pub fn depart(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "depart", "endpoint gone from the slice");
        self.views.record("orchestrator", ordinal, Belief::Departed, "depart");
        if let Some(registry) = self.views.proxy_registry() {
            registry.remove(ordinal);
            self.views.record("proxy", ordinal, Belief::Absent, "depart");
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.remove(ordinal);
            self.views.record(&observer, ordinal, Belief::PeerParked, "depart");
        }
    }

    /// **Re-admit `ordinal` at `addr`** — the counterpart of
    /// [`withdraw`](Self::withdraw): the proxy publishes the endpoint again at
    /// `addr` with health `Unknown` (nothing has probed it yet) and every other
    /// live worker's membership re-adds the peer.
    pub fn readmit(&mut self, ordinal: &str, addr: SocketAddr) {
        self.mark(ordinal, None, "readmit", &format!("endpoint published at {addr}"));
        self.views.record("orchestrator", ordinal, Belief::Admitted, "readmit");
        if let Some(registry) = self.views.proxy_registry() {
            registry.add(sip_proxy::registry::WorkerEntry {
                id: ordinal.to_string(),
                address: ProxyAddr::new(addr.ip().to_string(), addr.port()),
                health: WorkerHealth::Unknown,
                draining_since: None,
                first_seen_at_ms: None,
            });
            self.views.record(
                "proxy",
                ordinal,
                Belief::Registered(WorkerHealth::Unknown),
                "readmit",
            );
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.add(Peer::new(ordinal, ordinal));
            self.views.record(&observer, ordinal, Belief::PeerActive, "readmit");
        }
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.add(Peer::new(ordinal, ordinal));
        }
    }

    /// **Start a replacement incarnation of `ordinal` ALONGSIDE the running
    /// one** — the orchestrator recreating a withdrawn worker while its old
    /// process is still up. The new incarnation comes up at gen+1 on a fresh SIP
    /// address (a new pod IP, like a reboot) AND a fresh replication listen
    /// address, since the old incarnation still holds the declared one; the
    /// cluster's resolver map is re-pointed at it, so peers reach the
    /// replacement on their next connect (ADR-0012 D3). The old incarnation is
    /// untouched — the caller keeps its handle and decides when it dies.
    ///
    /// The replacement's SIP + repl binds are asserted, never assumed: a core
    /// whose replication listen fails is silently unreachable, which would read
    /// as a replication bug three assertions later.
    pub async fn spawn_replacement(&mut self, ordinal: &str) -> ReplicatedB2buaSut {
        let spec = self
            .worker_specs
            .get(ordinal)
            .cloned()
            .unwrap_or_else(|| panic!("worker {ordinal} was never spawned"));
        let gen = self.views.max_gen(ordinal) + 1;
        let octet = u8::try_from(gen).expect("gen fits a host octet (< 256 incarnations)");
        let sip_addr = SocketAddr::from(([127, 0, octet, 1], spec.sip_base_addr.port()));
        let declared = self.repl_addrs[ordinal];
        let listen_addr = SocketAddr::from((
            declared.ip(),
            declared.port() + 100 * u16::try_from(gen - 1).expect("gen fits"),
        ));
        // Peers must find the replacement, not the incarnation it replaces.
        self.repl_resolver.lock().unwrap().insert(ordinal.to_string(), listen_addr);

        let sut = self.spawn_incarnation(ordinal, &spec, gen, sip_addr, listen_addr).await;
        self.assert_repl_listening(ordinal, listen_addr).await;
        self.mark(
            ordinal,
            None,
            "replacement",
            &format!(
                "gen={gen} alongside the running incarnation; sip={sip_addr} repl={listen_addr}"
            ),
        );
        self.views.record("orchestrator", ordinal, Belief::Running { gen }, "spawn_replacement");
        sut
    }

    /// Assert `ordinal`'s new incarnation really owns `listen_addr` on the repl
    /// fabric. `B2buaCore` spawns its replication server in a task and swallows
    /// the listen error, so a duplicate bind would otherwise show up only as a
    /// peer that never pulls.
    async fn assert_repl_listening(&self, ordinal: &str, listen_addr: SocketAddr) {
        sip_clock::testkit::settle().await;
        match self.repl_sim.connect(listen_addr).await {
            Ok(_probe) => {}
            Err(e) => panic!(
                "{ordinal}'s replacement did not take its replication listen address                  {listen_addr} ({e:?}) — its peers would never reach it",
            ),
        }
    }

    /// **Begin a graceful drain of `node`** ([`B2buaCore::drain`]): latch
    /// `Draining` — so the proxy steers new calls away — and wait out its
    /// bounds. Returns the named exit + residual live-call count. The process
    /// keeps serving throughout.
    pub async fn begin_drain(
        &mut self,
        node: &ReplicatedB2buaSut,
        bounds: DrainBounds,
    ) -> DrainOutcome {
        self.mark(node.ordinal(), None, "drain", &format!("{bounds:?}"));
        node.begin_drain(bounds).await
    }

    /// **Begin a graceful drain of `node` WITHOUT waiting for it** — the same
    /// [`B2buaCore::drain`] the shutdown path runs, latched now and left in
    /// flight so the test keeps driving the timeline inside the drain window.
    /// Poll the returned [`PendingDrain`] after each advance to learn when the
    /// drain returned and with what residual.
    pub fn begin_drain_pending(
        &mut self,
        node: &ReplicatedB2buaSut,
        bounds: DrainBounds,
    ) -> PendingDrain {
        self.mark(node.ordinal(), None, "drain", &format!("{bounds:?}, not awaited"));
        node.begin_drain_detached(bounds)
    }

    /// The views ledger — every observer's belief about every worker, and when
    /// it moved (see [`crate::views`]).
    pub fn view_ledger(&self) -> &ViewLedger {
        &self.views
    }

    // -- clock -------------------------------------------------------------

    /// Advance the paused clock by `dur`, driving BOTH the SIP and repl sim
    /// pipelines with the proven settle/advance/settle discipline (CLAUDE.md).
    /// Drive the protocol BETWEEN advances: advance to the deadline, then assert.
    pub async fn advance(&self, dur: Duration) {
        sip_clock::testkit::pump_sampled(dur, || self.views.sample()).await;
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
    /// Run the FULL RFC audit suite — the per-bind **peer** rules (Via echo /
    /// response↔transaction correlation, tags, CANCEL/RAck correlation, …) on
    /// top of the cross-message rules — over the recorded trace and panic on
    /// any non-advisory finding, honouring [`allow_rfc_violation`] waivers.
    ///
    /// The Drop-time gate deliberately runs only the endpoint-scoped
    /// cross-message rules (a transparent failover splits one dialog's CSeq
    /// stream across worker binds, so per-bind peer rules would report phantom
    /// findings on the internal nodes). A NON-failover via-LB test — where no
    /// call ever changes workers — can and should opt into the full per-bind
    /// suite, which is exactly what gates the downstream e2e runs:
    /// it is the only lane that judges the **relay (proxy) bind's** own client
    /// transactions (e.g. `response-echoes-request-via` §8.1.3/§17.1.3 response matching).
    pub fn assert_full_rfc_clean(&self, cell: &str) {
        let events = self.harness.recording().channel().snapshot();
        // `offending` is a 1-based index into the AUDIT-visible wire entries — the
        // same view `evaluate_rfc_findings` hands the rules — so a window must be
        // resolved against `audit_wire_entries`, never the raw snapshot (whose
        // extra ReEmit / invisible-disposition rows shift every later index).
        let entries = sip_net::audit_wire_entries(&events);
        let findings: Vec<sip_net::RfcFinding> = sip_net::evaluate_rfc_findings(&events)
            .into_iter()
            .filter(|f| !f.advisory && !self.rfc_acceptance.waived(&f.rule))
            .filter(|f| !self.rfc_acceptance.accepts(&f.rule, f.offending, &entries))
            .collect();
        assert!(
            findings.is_empty(),
            "[{cell}] full-suite RFC audit violation(s) on the recorded trace:\n{}",
            findings
                .iter()
                .map(|f| format!("  • [{}] {}: {}", f.lane, f.rule, f.detail))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

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

    /// The RFC 3261 cross-message audit findings that GATE — everything the
    /// recorded trace violated except what an acceptance window covers (the
    /// `(lane, detail)` pairs `assert_sip_rfc_clean` panics on). Reads the
    /// recording channel snapshot NON-consuming, so it is safe to call mid-run AND
    /// from `Drop`. Empty ⇒ clean. Shared by the explicit `assert_sip_rfc_clean`
    /// and the automatic Drop-time enforcement so the SAME rule set runs on every
    /// FailoverHarness-based test with no per-test opt-in.
    fn rfc_audit_findings(&self) -> Vec<(String, String)> {
        lane_details(self.partition_rfc_findings().0)
    }

    /// Split the endpoint-scoped cross-message audit into `(gating, accepted)` —
    /// see [`RfcAcceptance::partition`].
    #[allow(clippy::type_complexity)]
    fn partition_rfc_findings(&self) -> (Vec<Finding>, Vec<Finding>) {
        self.rfc_acceptance.partition(&self.audited_events())
    }

    /// The findings an acceptance window covered this run — the deviations the
    /// scenario declared accepted via
    /// [`accept_rfc_deviations_from_now`](Self::accept_rfc_deviations_from_now).
    /// They do not gate, but they are recorded (advisory anomalies in the unified
    /// report) so an accepted trade-off stays visible, and a test can assert on
    /// them to prove its window matched something.
    pub fn accepted_rfc_deviations(&self) -> Vec<(String, String)> {
        lane_details(self.partition_rfc_findings().1)
    }

    /// The recorded SIP events the audit judges — the endpoint-scoped view.
    fn audited_events(&self) -> Vec<layer_harness::Stamped<sip_net::SignalingNetworkEvent>> {
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
        let incarnation_binds = self.views.all_sip_addrs();
        let worker_binds: std::collections::HashSet<String> = self
            .all_worker_sip_addrs
            .iter()
            .chain(self.relay_sip_addrs.iter())
            .chain(incarnation_binds.iter())
            .map(|a| a.to_string())
            .collect();
        let snapshot = self.harness.recording().channel().snapshot();
        snapshot
            .into_iter()
            .filter(|s| {
                !worker_binds.contains(s.event.bind_key()) && sip_net::audit_visible_event(&s.event)
            })
            .collect()
    }

    /// Record a rebooted worker's NEW SIP address into the RFC-audit exclusion
    /// set — the PRE-reboot incarnation's bind must stay excluded too, else the
    /// endpoint-scoped audit would mistake the worker's internal per-leg CSeq
    /// stream (split across incarnations) for an endpoint skip, or read its
    /// reclaimed in-dialog keepalive as a request outside any dialog. The
    /// report's own column follows the views ledger's incarnation registry, so
    /// nothing here is needed for the diagram.
    ///
    /// **Every reboot that re-points traffic at the new address calls this** —
    /// a worker bind the exclusion set does not name is audited as if it were a
    /// real UA, which [`audited_events`](Self::audited_events) exists to prevent.
    pub fn note_worker_rebound(&mut self, _ordinal: &str, new_addr: SocketAddr) {
        self.all_worker_sip_addrs.push(new_addr);
    }

    /// Snapshot the replication recording (captured frames + markers + lanes).
    pub fn repl_report(&self) -> ReplReport {
        let mut lanes: BTreeMap<SocketAddr, String> =
            self.repl_addrs.iter().map(|(ord, addr)| (*addr, ord.clone())).collect();
        // A replacement listens elsewhere than its ordinal's declared address —
        // name every incarnation's listen addr so no repl lane renders raw.
        for axis in self.views.axes() {
            lanes.insert(axis.repl_addr, axis.ordinal.clone());
        }
        ReplReport { frames: self.repl_recording.captured(), markers: self.markers.clone(), lanes }
    }

    /// The worker axes for the unified report's combiner — one column per
    /// ordinal, or one per INCARNATION for an ordinal that ran two at once (a
    /// replacement), grouped under the ordinal. Ordered by declaration (the
    /// repl-addr port order, so the columns read `b1, b2`), incarnations of one
    /// ordinal kept adjacent so their group bracket is contiguous.
    fn worker_axes(&self) -> Vec<crate::combine::WorkerAxis> {
        let mut axes = self.views.axes();
        axes.sort_by_key(|a| {
            (self.repl_addrs.get(&a.ordinal).map(|d| d.port()).unwrap_or(u16::MAX), a.gen)
        });
        axes.into_iter()
            .map(|a| crate::combine::WorkerAxis {
                id: if a.fanned { format!("{}#g{}", a.ordinal, a.gen) } else { a.ordinal.clone() },
                group: a.fanned.then(|| a.ordinal.clone()),
                ordinal: a.ordinal,
                sip_addr: a.sip_addr,
                repl_addr: a.repl_addr,
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
            &self.views.views(),
        );
        // RFC 3261 status MUST be reflected in the report: a trace that violates a
        // gating rule can NEVER show PASS. Fold the findings into the doc anomalies
        // and force passed=false so the rendered report.html / global.txt show FAIL
        // and list the violation(s). A window-ACCEPTED deviation is listed too, as
        // advisory: classified, not masked, and it does not fail the run.
        let (gating, accepted) = self.partition_rfc_findings();
        if !gating.is_empty() {
            doc.passed = false;
        }
        for (rule, lane, detail) in gating {
            doc.anomalies.push(seq_report::Anomaly {
                check: rule,
                detail,
                lane: Some(lane),
                endpoint: None,
                advisory: Some(false),
                row_seqs: Vec::new(),
                rule_sourced: true,
            });
        }
        for (rule, lane, detail) in accepted {
            doc.anomalies.push(seq_report::Anomaly {
                check: rule,
                detail: format!("ACCEPTED (declared deviation window): {detail}"),
                lane: Some(lane),
                endpoint: None,
                advisory: Some(true),
                row_seqs: Vec::new(),
                rule_sourced: true,
            });
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
        if self.harness.inner.lock().map(|g| g.is_none()).unwrap_or(true) {
            return;
        }
        let dir = Self::seq_reports_dir().join(Self::sanitize_name(&self.name));
        // unified_doc + render + write are all sync; swallow any IO error.
        let _ = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let doc = self.unified_doc(&self.name, true);
            std::fs::write(dir.join("report.html"), seq_report::render_html(&doc))?;
            std::fs::write(dir.join("report.global.txt"), seq_report::render_global_txt(&doc))?;
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
        if self.harness.inner.lock().map(|g| g.is_none()).unwrap_or(true) {
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
        let h = self.inner.lock().unwrap().take().expect("harness taken (already finished?)");
        let a = h.agent(name, addr).await;
        *self.inner.lock().unwrap() = Some(h);
        a
    }

    /// [`agent`](Self::agent) with a pre-ingress hook on the UA's bind.
    async fn agent_with_pre_ingress(
        &self,
        name: &str,
        addr: &str,
        hook: sip_net::PreIngressHook,
    ) -> Agent {
        let h = self.inner.lock().unwrap().take().expect("harness taken (already finished?)");
        let a = h.agent_with_pre_ingress(name, addr, hook).await;
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
