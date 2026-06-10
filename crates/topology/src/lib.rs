//! topology — the shared, **port-agnostic** cluster-membership seam (slice S1a
//! of the HA-replication migration).
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
//!   AddressChanged`, with **no** health transition. Health is a proxy-layer
//!   classification (OPTIONS-probe-driven) that the proxy composes *over*
//!   membership (its `WorkerSet` annotation overlay); modelling it here would leak
//!   that concern into the b2bua replication path that has no notion of OPTIONS
//!   health.
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

/// A cluster member: identity ordinal + reachability host. **Port-agnostic** —
/// membership carries no transport port (that is a consumer concern). Cf. the
/// proxy's `WorkerEntry`, minus `address.port`, `health`, and the LB timing
/// stamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub ordinal: Ordinal,
    pub host: String,
}

impl Peer {
    pub fn new(ordinal: impl Into<Ordinal>, host: impl Into<String>) -> Self {
        Self { ordinal: ordinal.into(), host: host.into() }
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
    /// let a rebooting node latch Ready before a single reclaim puller existed.
    fn synced(&self) -> bool {
        true
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
    /// is the new host; `peer.ordinal` selects the target.
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
                    p.host = peer.host;
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
/// host moved, and **nothing** for an unchanged peer. This is the pure heart of
/// every snapshot-driven membership source (the k8s informer feeds it the set
/// it derived from EndpointSlices) — testable with no cluster, just a
/// [`MembershipState`] and its `changes()` receiver.
///
/// `desired` may arrive in any order and may contain duplicate ordinals (two
/// EndpointSlices listing the same pod); the first occurrence of each ordinal
/// wins and the rest are ignored, mirroring the snapshot's de-dup.
pub fn reconcile_to_desired(state: &MembershipState, desired: Vec<Peer>) {
    // De-dup desired by ordinal (first wins), preserving a stable target set.
    let mut seen = std::collections::HashSet::new();
    let desired: Vec<Peer> = desired.into_iter().filter(|p| seen.insert(p.ordinal.clone())).collect();
    let desired_ordinals: std::collections::HashSet<&str> =
        desired.iter().map(|p| p.ordinal.as_str()).collect();

    // Removals: a current ordinal no longer desired.
    for cur in state.snapshot() {
        if !desired_ordinals.contains(cur.ordinal.as_str()) {
            state.remove(&cur.ordinal);
        }
    }
    // Adds + address changes (each mutator is a no-op when nothing changed, so
    // an unchanged peer emits no delta).
    for d in desired {
        match state.resolve(&d.ordinal) {
            None => state.add(d),
            Some(cur) if cur.host != d.host => state.set_address(d),
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
/// reconcile and KEEP LOOPING; the old `Err(_) => return` here is exactly what
/// deafened the supervisor and stranded it on dead peer IPs (ADR-0012 D1). Only a
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
/// (mirrors the proxy's `SimulatedWorkerRegistry`); the clock is for future
/// timestamp needs and to keep the seam uniform with the proxy's simulated
/// registry — membership mutations themselves carry no behavioural time.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordinals(peers: &[Peer]) -> Vec<&str> {
        peers.iter().map(|p| p.ordinal.as_str()).collect()
    }

    #[tokio::test]
    async fn snapshot_reflects_added_peers_in_deterministic_order() {
        let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
        // Add out of ordinal order; snapshot must come back sorted.
        m.add(Peer::new("w2", "h2"));
        m.add(Peer::new("w0", "h0"));
        m.add(Peer::new("w1", "h1"));
        assert_eq!(ordinals(&m.snapshot()), vec!["w0", "w1", "w2"]);
    }

    #[tokio::test]
    async fn changes_observes_added_removed_address_changed_in_order() {
        let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
        let mut rx = m.changes();

        m.add(Peer::new("w0", "h0"));
        m.change_address(Peer::new("w0", "h0-new"));
        m.remove("w0");

        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "h0")));
        assert_eq!(
            rx.try_recv().unwrap(),
            MemberDelta::AddressChanged(Peer::new("w0", "h0-new"))
        );
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w0".to_string()));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn no_op_mutations_emit_nothing() {
        let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
        let mut rx = m.changes();
        // Unchanged host → no AddressChanged.
        m.change_address(Peer::new("w0", "h0"));
        // Unknown ordinal → no delta on either path.
        m.change_address(Peer::new("ghost", "h"));
        m.remove("ghost");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn scale_up_then_down() {
        let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
        let mut rx = m.changes();

        m.add(Peer::new("w0", "h0"));
        m.add(Peer::new("w1", "h1"));
        m.add(Peer::new("w2", "h2"));
        m.remove("w1");

        assert_eq!(ordinals(&m.snapshot()), vec!["w0", "w2"]);

        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "h0")));
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w1", "h1")));
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w2", "h2")));
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w1".to_string()));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn restart_remove_then_readd_same_ordinal_new_host() {
        let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
        let mut rx = m.changes();

        m.remove("w0");
        m.add(Peer::new("w0", "h0-restarted"));

        // Restart is observed as Removed then Added (not AddressChanged) — the
        // ordinal genuinely left and re-joined.
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w0".to_string()));
        assert_eq!(
            rx.try_recv().unwrap(),
            MemberDelta::Added(Peer::new("w0", "h0-restarted"))
        );
        assert_eq!(m.resolve("w0").unwrap().host, "h0-restarted");
        // Single entry — re-add replaced, not duplicated.
        assert_eq!(m.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn readd_existing_ordinal_replaces_not_duplicates() {
        let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
        m.add(Peer::new("w0", "h0-v2"));
        assert_eq!(m.snapshot(), vec![Peer::new("w0", "h0-v2")]);
    }

    #[tokio::test]
    async fn static_membership_parses_and_yields_stable_snapshot() {
        let m = StaticMembership::from_string("w1@host1, w0@host0", "test").unwrap();
        let snap = m.snapshot();
        // Parsed two peers, returned sorted by ordinal regardless of input order.
        assert_eq!(
            snap,
            vec![Peer::new("w0", "host0"), Peer::new("w1", "host1")]
        );
        // changes() never fires but is a live receiver.
        let mut rx = m.changes();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn static_membership_empty_string_is_empty_set() {
        assert!(StaticMembership::from_string("   ", "test").unwrap().snapshot().is_empty());
    }

    #[test]
    fn parse_peer_list_rejects_malformed() {
        assert!(parse_peer_list("t", "noatsign").is_err());
        assert!(parse_peer_list("t", "@host").is_err());
        assert!(parse_peer_list("t", "id@").is_err());
        assert!(parse_peer_list("t", "a@h,a@h2").is_err()); // duplicate ordinal
        assert!(parse_peer_list("t", "a@h,,b@h2").is_err()); // empty entry
    }

    #[tokio::test]
    async fn reconcile_emits_add_remove_addresschanged_and_nothing_for_unchanged() {
        let state = MembershipState::new(vec![Peer::new("w0", "h0"), Peer::new("w1", "h1")]);
        let mut rx = state.changes();

        // Desired: w0 unchanged, w1 moved hosts, w2 new, (w-implicit removed: none).
        reconcile_to_desired(
            &state,
            vec![Peer::new("w1", "h1-new"), Peer::new("w0", "h0"), Peer::new("w2", "h2")],
        );
        assert_eq!(
            ordinals(&state.snapshot()),
            vec!["w0", "w1", "w2"],
            "w2 joined, none removed"
        );
        // w0 unchanged → no delta; w1 moved → AddressChanged; w2 → Added. Order:
        // removals first (none), then adds/changes in desired order (w1, w0(skip), w2).
        assert_eq!(
            rx.try_recv().unwrap(),
            MemberDelta::AddressChanged(Peer::new("w1", "h1-new"))
        );
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w2", "h2")));
        assert!(rx.try_recv().is_err(), "w0 unchanged emits nothing");
    }

    #[tokio::test]
    async fn reconcile_removes_departed_ordinals() {
        let state = MembershipState::new(vec![Peer::new("w0", "h0"), Peer::new("w1", "h1")]);
        let mut rx = state.changes();
        // w1 vanished from the desired set (pod deleted).
        reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
        assert_eq!(ordinals(&state.snapshot()), vec!["w0"]);
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w1".to_string()));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconcile_dedups_duplicate_ordinals_first_wins() {
        // Two EndpointSlices listing the same pod → one Peer, no churn.
        let state = MembershipState::new(vec![]);
        let mut rx = state.changes();
        reconcile_to_desired(
            &state,
            vec![Peer::new("w0", "10.0.0.1"), Peer::new("w0", "10.0.0.2")],
        );
        assert_eq!(state.snapshot(), vec![Peer::new("w0", "10.0.0.1")]);
        assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "10.0.0.1")));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconcile_is_idempotent() {
        let state = MembershipState::new(vec![Peer::new("w0", "h0")]);
        reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
        let mut rx = state.changes();
        // Re-applying the same desired set is a no-op (no spurious deltas).
        reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn parse_peer_list_accepts_host_with_no_port() {
        // Port-agnostic: a bare host, an FQDN, even a host:port-looking string
        // is taken whole as the host (no port split).
        let peers = parse_peer_list("t", "w0@10.0.0.1, w1@b2b.svc.cluster.local").unwrap();
        assert_eq!(peers[0].host, "10.0.0.1");
        assert_eq!(peers[1].host, "b2b.svc.cluster.local");
    }
}
