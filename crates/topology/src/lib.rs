//! topology — the shared, **port-agnostic** cluster-membership seam of HA
//! replication.
//!
//! Both the front proxy (worker LB) and the b2bua replication engine need to
//! agree on *who is in the cluster*. That "who" is membership: a set of
//! [`Peer`]s, each an `ordinal` identity + a `host`. It is deliberately a strict
//! subset of the proxy's `WorkerRegistry`:
//!
//! - **No transport port.** A `Peer` is `ordinal@host`, not `ordinal@host:port`.
//!   Membership is about identity + reachability address; *which* port carries
//!   SIP vs. the replication stream is a consumer concern layered on top.
//! - **No health.** [`MemberDelta`] is membership-only — `Added | Removed |
//!   AddressChanged | ConditionsChanged`, with **no** health transition. Health is
//!   a proxy-layer classification (OPTIONS-probe-driven) that the proxy composes
//!   *over* membership (its `WorkerSet` annotation overlay); modelling it here
//!   would leak that concern into the b2bua replication path that has no notion
//!   of OPTIONS health. A `Peer`'s `ready` / `terminating` are the
//!   **orchestrator's endpoint conditions** (what the EndpointSlice says), not a
//!   probe verdict: one snapshot, two predicates — *pullable* is "present",
//!   *routable* is `ready` (ADR-0031 D1).
//!
//! The backing [`MembershipState`] is the proxy's worker-set source of truth (the
//! proxy's `WorkerSet` composes a `WorkerEntry` view as membership ⊕ health): an
//! [`arc_swap::ArcSwap`] snapshot read on the hot path + a
//! [`tokio::sync::broadcast`] of deltas (no backfill — subscribers `snapshot`
//! first). Snapshots are kept **sorted by ordinal** so every consumer sees a
//! deterministic, stable order regardless of insertion sequence.
//!
//! ADR-0002 acyclicity: this is a leaf crate (it depends only on tokio /
//! arc-swap / thiserror / the leaf `sip-clock`). proxy → topology and
//! b2bua → topology; never the reverse.

use std::sync::Arc;

use arc_swap::ArcSwap;
use sip_clock::Clock;
use tokio::sync::broadcast;

#[cfg(feature = "kube")]
pub mod k8s;
#[cfg(feature = "kube")]
pub use k8s::K8sMembership;

/// Peer identity ordinal. A plain `String` — the **same** identity type the
/// proxy's `WorkerEntry.id` uses (`WorkerId = String`) — so a `Peer.ordinal`
/// and a `WorkerEntry.id` are directly comparable when the proxy layers its
/// health view over this membership set. The membership impls reject
/// empty/malformed ordinals at build time.
pub type Ordinal = String;

/// A cluster member: identity ordinal + reachability host + the orchestrator's
/// endpoint conditions. **Port-agnostic** — membership carries no transport port
/// (that is a consumer concern). Cf. the proxy's `WorkerEntry`, minus
/// `address.port`, `health`, and the LB timing stamps.
///
/// A member is **pullable** while it is in the snapshot at all, whatever its
/// conditions; it is **routable** only while `ready` (ADR-0031 D1). A condition
/// flip is a [`MemberDelta::ConditionsChanged`], never a departure or a host
/// move, so a consumer keyed on presence (the replication supervisor) keeps its
/// pullers and their watermarks across it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub ordinal: Ordinal,
    pub host: String,
    /// The endpoint's `ready` condition: the member serves new traffic. An
    /// absent condition reads as not ready.
    pub ready: bool,
    /// The endpoint's `terminating` condition: the orchestrator has begun
    /// removing the member; it is still a replication peer until it leaves.
    pub terminating: bool,
}

impl Peer {
    /// A ready, non-terminating member — the steady-state shape.
    pub fn new(ordinal: impl Into<Ordinal>, host: impl Into<String>) -> Self {
        Self { ordinal: ordinal.into(), host: host.into(), ready: true, terminating: false }
    }

    /// A member with explicit endpoint conditions.
    pub fn with_conditions(
        ordinal: impl Into<Ordinal>,
        host: impl Into<String>,
        ready: bool,
        terminating: bool,
    ) -> Self {
        Self { ordinal: ordinal.into(), host: host.into(), ready, terminating }
    }

    /// The same member with `ready = false`.
    pub fn not_ready(mut self) -> Self {
        self.ready = false;
        self
    }

    /// The same member with `terminating = true`.
    pub fn terminating(mut self) -> Self {
        self.terminating = true;
        self
    }

    /// Whether `other` is the same member at the same host with different
    /// conditions.
    fn conditions_differ(&self, other: &Peer) -> bool {
        self.ready != other.ready || self.terminating != other.terminating
    }
}

/// Tagged membership delta emitted on observable change. Membership-only: **no**
/// health transition (health is a proxy-layer concern composed on top).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberDelta {
    /// A peer joined (or a removed ordinal re-joined).
    Added(Peer),
    /// The peer with this ordinal left.
    Removed(Ordinal),
    /// An existing peer's host changed (same ordinal, new host).
    AddressChanged(Peer),
    /// An existing peer's endpoint conditions changed (same ordinal, same host,
    /// `ready` and/or `terminating` flipped). Presence is unchanged: the member
    /// stays a replication peer; only its routability moved (ADR-0031 D1).
    ConditionsChanged(Peer),
}

/// The read seam consumers (proxy LB, b2bua replication engine) depend on.
/// Reads are sync + lock-free; `changes()` is a delta subscription with no
/// backfill — subscribe, then `snapshot`.
pub trait Membership: Send + Sync {
    /// Snapshot the current peer set (sorted by ordinal — deterministic order).
    fn snapshot(&self) -> Vec<Peer>;
    /// Subscribe to membership deltas from this point on (no backfill).
    fn changes(&self) -> broadcast::Receiver<MemberDelta>;
    /// Whether this source's view is **authoritative** yet. Static/simulated
    /// memberships are authoritative at construction (default `true`); an
    /// informer-backed source (`K8sMembership`) starts with an EMPTY snapshot
    /// and flips this only once its initial LIST completes. Consumers gating
    /// vacuous-truth decisions over the peer set (the b2bua readiness gates: an
    /// empty set is "ready" ONLY if the empty view is real) MUST check this —
    /// treating the informer's pre-sync empty snapshot as a peerless cluster
    /// lets a rebooting node latch Ready before a single reclaim puller existed.
    fn synced(&self) -> bool {
        true
    }
    /// Whether this source shows a member **its own** endpoint, so a worker can
    /// observe its own withdrawal from routing (ADR-0031 D6). True for an
    /// informer over the pool the worker belongs to (and its simulation); false
    /// for a static list, which is fixed by hand and withdraws nobody.
    fn observes_self(&self) -> bool {
        false
    }
}

/// Layer-build-time parse failure for the `ordinal@host,...` grammar.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("static membership parse error ({origin}): {reason}")]
pub struct MembershipParseError {
    pub origin: String,
    pub reason: String,
}

/// Shared lock-free state backing both the read seam and the mutators. The
/// static + simulated memberships are thin wrappers over this; the proxy's
/// `WorkerSet` composes its `WorkerEntry` view directly over a `Membership`.
pub struct MembershipState {
    peers: ArcSwap<Vec<Peer>>,
    tx: broadcast::Sender<MemberDelta>,
}

impl MembershipState {
    /// Build from an initial peer set (normalised to sorted-by-ordinal order).
    pub fn new(initial: Vec<Peer>) -> Self {
        let mut initial = initial;
        sort_by_ordinal(&mut initial);
        let (tx, _rx) = broadcast::channel(256);
        Self { peers: ArcSwap::from_pointee(initial), tx }
    }

    /// Snapshot the current peer set (already sorted by ordinal).
    pub fn snapshot(&self) -> Vec<Peer> {
        self.peers.load().as_ref().clone()
    }

    /// Resolve a peer by ordinal (`None` if absent).
    pub fn resolve(&self, ordinal: &str) -> Option<Peer> {
        self.peers.load().iter().find(|p| p.ordinal == ordinal).cloned()
    }

    /// Subscribe to deltas from this point on (no backfill).
    pub fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        self.tx.subscribe()
    }

    /// Replace the set with `f(current)` (re-sorted by ordinal afterwards) and
    /// emit `delta` (best-effort — a dropped delta with no subscribers is fine,
    /// `changes` has no backfill).
    pub fn mutate(&self, f: impl FnOnce(&mut Vec<Peer>), delta: MemberDelta) {
        let mut next = self.snapshot();
        f(&mut next);
        sort_by_ordinal(&mut next);
        self.peers.store(Arc::new(next));
        let _ = self.tx.send(delta);
    }

    /// Add (or replace) a peer, emitting [`MemberDelta::Added`]. If a peer with
    /// the same ordinal already exists it is replaced (the join-after-restart
    /// path; consumers see a fresh `Added` for the re-joined ordinal).
    pub fn add(&self, peer: Peer) {
        let delta = MemberDelta::Added(peer.clone());
        self.mutate(
            |peers| {
                peers.retain(|p| p.ordinal != peer.ordinal);
                peers.push(peer);
            },
            delta,
        );
    }

    /// Remove the peer with `ordinal`, emitting [`MemberDelta::Removed`] (no-op
    /// if absent — no spurious delta).
    pub fn remove(&self, ordinal: &str) {
        if self.resolve(ordinal).is_none() {
            return;
        }
        self.mutate(
            |peers| peers.retain(|p| p.ordinal != ordinal),
            MemberDelta::Removed(ordinal.to_string()),
        );
    }

    /// Change an existing peer's host, emitting [`MemberDelta::AddressChanged`]
    /// (no-op if the ordinal is unknown or the host is unchanged). `peer.host`
    /// is the new host; `peer.ordinal` selects the target. The peer's conditions
    /// are taken from `peer` too: a host move is a fresh endpoint, described whole.
    pub fn set_address(&self, peer: Peer) {
        let Some(cur) = self.resolve(&peer.ordinal) else {
            return;
        };
        if cur.host == peer.host {
            return;
        }
        let delta = MemberDelta::AddressChanged(peer.clone());
        self.mutate(
            |peers| {
                if let Some(p) = peers.iter_mut().find(|p| p.ordinal == peer.ordinal) {
                    *p = peer;
                }
            },
            delta,
        );
    }

    /// Change an existing peer's endpoint conditions, emitting
    /// [`MemberDelta::ConditionsChanged`] (no-op if the ordinal is unknown, the
    /// host differs, or the conditions are unchanged). `peer.ordinal` selects the
    /// target; `peer.ready` / `peer.terminating` are the new conditions.
    pub fn set_conditions(&self, peer: Peer) {
        let Some(cur) = self.resolve(&peer.ordinal) else {
            return;
        };
        if cur.host != peer.host || !cur.conditions_differ(&peer) {
            return;
        }
        let delta = MemberDelta::ConditionsChanged(peer.clone());
        self.mutate(
            |peers| {
                if let Some(p) = peers.iter_mut().find(|p| p.ordinal == peer.ordinal) {
                    p.ready = peer.ready;
                    p.terminating = peer.terminating;
                }
            },
            delta,
        );
    }
}

/// Stable snapshot ordering: sort by ordinal so consumers see deterministic
/// order regardless of insertion sequence.
fn sort_by_ordinal(peers: &mut [Peer]) {
    peers.sort_by(|a, b| a.ordinal.cmp(&b.ordinal));
}

/// Reconcile `state` to `desired` (the full membership the watcher just
/// observed), emitting exactly the deltas that close the gap:
/// [`MemberDelta::Removed`] for ordinals that left, [`MemberDelta::Added`] for
/// new ordinals, [`MemberDelta::AddressChanged`] for an existing ordinal whose
/// host moved, [`MemberDelta::ConditionsChanged`] for an existing ordinal at the
/// same host whose `ready` / `terminating` flags flipped, and **nothing** for an
/// unchanged peer. This is the pure heart of every snapshot-driven membership
/// source (the k8s informer feeds it the set it derived from EndpointSlices) —
/// testable with no cluster, just a [`MembershipState`] and its `changes()`
/// receiver.
///
/// `desired` may arrive in any order. A duplicate ordinal is pure de-dup — the
/// first occurrence wins, the rest are ignored — so a source that can list one
/// member twice with different conditions must merge them before calling
/// (`peers_from_slices` does).
pub fn reconcile_to_desired(state: &MembershipState, desired: Vec<Peer>) {
    // De-dup desired by ordinal (first wins), preserving a stable target set.
    let mut seen = std::collections::HashSet::new();
    let desired: Vec<Peer> =
        desired.into_iter().filter(|p| seen.insert(p.ordinal.clone())).collect();
    let desired_ordinals: std::collections::HashSet<&str> =
        desired.iter().map(|p| p.ordinal.as_str()).collect();

    // Removals: a current ordinal no longer desired.
    for cur in state.snapshot() {
        if !desired_ordinals.contains(cur.ordinal.as_str()) {
            state.remove(&cur.ordinal);
        }
    }
    // Adds, address changes, condition flips (each mutator is a no-op when
    // nothing changed, so an unchanged peer emits no delta).
    for d in desired {
        match state.resolve(&d.ordinal) {
            None => state.add(d),
            Some(cur) if cur.host != d.host => state.set_address(d),
            Some(cur) if cur.conditions_differ(&d) => state.set_conditions(d),
            Some(_) => {}
        }
    }
}

/// Drive a `reconcile` closure from a [`Membership`] source — the single
/// broadcast-consume loop shared by the proxy worker registry (ADR-0012 D4) and
/// the b2bua replication supervisor (D1/D2), each supplying its own projection
/// of the snapshot onto its registry/puller set.
///
/// Subscribes BEFORE the initial snapshot (no lost delta — `changes()` has no
/// backfill), runs `reconcile(snapshot)` once **synchronously** (so the caller's
/// state is populated before this returns), then spawns a task that re-reconciles
/// from the **authoritative snapshot** on every wakeup: a delta, a `Lagged`
/// overflow, or a `period` tick. A `Lagged` (we fell behind a bursty producer and
/// dropped intermediate deltas) is non-fatal — the next snapshot is current, so we
/// reconcile and KEEP LOOPING — returning on it would deafen the supervisor and
/// strand it on dead peer IPs (ADR-0012 D1). Only a
/// `Closed` source (membership dropped) stops the loop. Returns the task handle
/// (abort to stop). `reconcile` must be idempotent — an unchanged set is a no-op.
pub fn spawn_membership_reconcile<F>(
    membership: Arc<dyn Membership>,
    period: std::time::Duration,
    mut reconcile: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut(Vec<Peer>) + Send + 'static,
{
    // Subscribe BEFORE the snapshot so no delta between snapshot and subscribe is
    // lost; the initial reconcile runs synchronously so the caller's state is
    // current the moment this returns.
    let mut changes = membership.changes();
    reconcile(membership.snapshot());
    tokio::spawn(async move {
        // The periodic belt-and-suspenders reconcile (ADR-0012 D2). `interval`'s
        // first tick fires immediately — consume it; the synchronous initial
        // reconcile above already covered the boot snapshot.
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            // Every wakeup — a delta, a `Lagged` overflow, or the periodic tick —
            // takes the same path: reconcile from the authoritative snapshot. Only
            // `Closed` (source gone) stops us.
            let proceed = tokio::select! {
                r = changes.recv() => !matches!(r, Err(broadcast::error::RecvError::Closed)),
                _ = ticker.tick() => true,
            };
            if !proceed {
                return;
            }
            reconcile(membership.snapshot());
        }
    })
}

/// Parse `ordinal@host,ordinal@host,...` into a peer set. An empty/blank string
/// yields an empty set. Rejects empty entries, missing/edge `@`, empty
/// ordinals, empty hosts, and duplicate ordinals. **Port-agnostic** — the host
/// part is taken whole (no `:port` split); mirrors the proxy's
/// `parse_worker_list` minus the `host:port` parse.
pub fn parse_peer_list(source: &str, raw: &str) -> Result<Vec<Peer>, MembershipParseError> {
    let err = |reason: String| MembershipParseError { origin: source.to_string(), reason };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part_raw in trimmed.split(',') {
        let part = part_raw.trim();
        if part.is_empty() {
            return Err(err(format!("empty entry in {source}")));
        }
        match part.find('@') {
            // No `@`, or leading `@`, are invalid.
            Some(0) | None => {
                return Err(err(format!("entry \"{part}\" must be of the form ordinal@host")))
            }
            Some(at) => {
                let ordinal = part[..at].trim();
                let host = part[at + 1..].trim();
                if ordinal.is_empty() {
                    return Err(err(format!("empty ordinal in entry \"{part}\"")));
                }
                if host.is_empty() {
                    return Err(err(format!("empty host in entry \"{part}\"")));
                }
                if !seen.insert(ordinal.to_string()) {
                    return Err(err(format!("duplicate ordinal \"{ordinal}\"")));
                }
                out.push(Peer::new(ordinal, host));
            }
        }
    }
    Ok(out)
}

/// A fixed peer set (dev/local wiring + tests). `snapshot` is a lock-free read;
/// `changes()` is an empty (never-firing) subscription kept alive by the
/// retained sender. Mirrors the proxy's `StaticWorkerRegistry`.
pub struct StaticMembership {
    state: MembershipState,
}

impl StaticMembership {
    /// Build from an inline `ordinal@host,...` string. Fails on malformed input.
    pub fn from_string(raw: &str, source: &str) -> Result<Self, MembershipParseError> {
        Ok(Self { state: MembershipState::new(parse_peer_list(source, raw)?) })
    }

    /// Build directly from peers (programmatic wiring/tests).
    pub fn from_peers(peers: Vec<Peer>) -> Self {
        Self { state: MembershipState::new(peers) }
    }
}

impl Membership for StaticMembership {
    fn snapshot(&self) -> Vec<Peer> {
        self.state.snapshot()
    }
    fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        // Never fires, but the retained sender keeps the receiver alive.
        self.state.changes()
    }
}

/// A membership whose peer set is driven imperatively by a test. Cheap to clone
/// — clones share the same underlying state + delta channel. Clock-injected
/// (mirrors the proxy's `SimulatedWorkerRegistry`); membership mutations
/// themselves carry no behavioural time.
#[derive(Clone)]
pub struct SimulatedMembership {
    state: Arc<MembershipState>,
    #[allow(dead_code)]
    clock: Clock,
}

impl SimulatedMembership {
    /// Build with the system clock.
    pub fn new(initial: Vec<Peer>) -> Self {
        Self { state: Arc::new(MembershipState::new(initial)), clock: Clock::system() }
    }

    /// Build with an injected clock (tests use `Clock::test_at(..)`).
    pub fn with_clock(initial: Vec<Peer>, clock: Clock) -> Self {
        Self { state: Arc::new(MembershipState::new(initial)), clock }
    }

    /// Add (or replace) a peer, emitting [`MemberDelta::Added`].
    pub fn add(&self, peer: Peer) {
        self.state.add(peer);
    }

    /// Remove the peer with `ordinal`, emitting [`MemberDelta::Removed`] (no-op
    /// if absent).
    pub fn remove(&self, ordinal: &str) {
        self.state.remove(ordinal);
    }

    /// Change an existing peer's host, emitting [`MemberDelta::AddressChanged`]
    /// (no-op if unknown or unchanged).
    pub fn change_address(&self, peer: Peer) {
        self.state.set_address(peer);
    }

    /// Set an existing peer's endpoint conditions, emitting
    /// [`MemberDelta::ConditionsChanged`] (no-op if unknown or unchanged). The
    /// peer stays in the snapshot: it is still pulled, and routable iff `ready`
    /// (ADR-0031 D1).
    pub fn set_conditions(&self, ordinal: &str, ready: bool, terminating: bool) {
        let Some(cur) = self.state.resolve(ordinal) else {
            return;
        };
        self.state.set_conditions(Peer::with_conditions(ordinal, cur.host, ready, terminating));
    }

    /// Resolve a peer by ordinal (test introspection).
    pub fn resolve(&self, ordinal: &str) -> Option<Peer> {
        self.state.resolve(ordinal)
    }
}

impl Membership for SimulatedMembership {
    fn snapshot(&self) -> Vec<Peer> {
        self.state.snapshot()
    }
    fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        self.state.changes()
    }
    /// The simulation stands in for the informer: a node's own ordinal is in
    /// its snapshot while the orchestrator publishes it.
    fn observes_self(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
