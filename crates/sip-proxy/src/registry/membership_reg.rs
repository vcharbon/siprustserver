//! [`MembershipWorkerRegistry`] — a [`WorkerRegistry`] driven by a
//! [`topology::Membership`] source (ADR-0012 D4). In production the source is the
//! `topology::K8sMembership` EndpointSlice informer — the **same** watch the
//! b2bua replication engine consumes (ADR-0011 X7, finally realized for the
//! proxy). It replaces the deploy-time baked-IP [`StaticWorkerRegistry`]: a worker
//! reboot/scale flows through the watch automatically, with **no `PROXY_WORKERS`
//! refresh / proxy redeploy**.
//!
//! ## Two health signals, kept separate
//! Membership (the *set* + each worker's **Pod IP** + k8s readiness) comes from
//! the informer; liveness/load health (`Alive`/`NotReady`/`Draining`/`Dead` +
//! bands) stays the OPTIONS [`HealthProbe`](crate::health::HealthProbe), written
//! through the [`control`](MembershipWorkerRegistry::control) seam. The two must
//! not fight: the reconcile is **health-preserving** — an unchanged worker's
//! probe-written health is never touched; only genuinely new/removed/moved
//! endpoints mutate the set. A new worker enters [`WorkerHealth::Unknown`] (not
//! routable for new dialogs) until its first OPTIONS flips it `Alive`.
//!
//! The proxy still reaches workers by their **direct Pod IP** (the informer's
//! `host`); no DNS in the data path. Every wakeup — a delta, a `Lagged` overflow,
//! or the periodic tick — reconciles from the **authoritative snapshot**, so a
//! missed/lagged delta self-heals exactly as the repl supervisor does (D1/D2).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use tokio::sync::broadcast;
use topology::{Membership, Peer};

use crate::addr::ProxyAddr;

use super::control::{RegistryStateControl, WorkerRegistryControl};
use super::{RegistryState, WorkerEntry, WorkerHealth, WorkerRegistry};

/// Cadence of the belt-and-suspenders snapshot reconcile (ADR-0012 D2/D4): any
/// missed delta self-heals; an unchanged set is a no-op.
const RECONCILE_PERIOD: Duration = Duration::from_secs(5);

/// A worker registry materialised from a live [`Membership`] source. Cheap to
/// share via the `WorkerRegistry` trait object; the background informer-consuming
/// task is aborted on drop.
pub struct MembershipWorkerRegistry {
    state: Arc<RegistryState>,
    clock: Clock,
    task: tokio::task::JoinHandle<()>,
}

impl MembershipWorkerRegistry {
    /// Materialise an initial snapshot from `membership`, then spawn a task that
    /// keeps the worker set in step with the membership stream. `sip_port` is the
    /// (cluster-wide) SIP port appended to each peer's Pod IP — membership is
    /// port-agnostic, the port is the proxy's concern.
    pub fn spawn(membership: Arc<dyn Membership>, sip_port: u16, clock: Clock) -> Self {
        // Start empty; the shared driver's synchronous initial reconcile populates
        // the set from the boot snapshot before this returns. Every wakeup — a
        // delta, a `Lagged` overflow, or the periodic tick — runs the same
        // health-preserving `reconcile_to_desired` from the AUTHORITATIVE snapshot,
        // so a missed/lagged delta self-heals exactly as the repl supervisor does
        // (ADR-0012 D1/D2). The driver itself lives in `topology` so the proxy and
        // the b2bua supervisor share one copy of that loop.
        let state = Arc::new(RegistryState::new(vec![]));
        let st = state.clone();
        let clk = clock.clone();
        let task = topology::spawn_membership_reconcile(membership, RECONCILE_PERIOD, move |snapshot| {
            let now = clk.now_ms().max(0) as u64;
            reconcile_to_desired(&st, snapshot, sip_port, now);
        });

        Self { state, clock, task }
    }

    /// A health-write [`WorkerRegistryControl`] over this registry's shared state —
    /// the OPTIONS [`HealthProbe`](crate::health::HealthProbe) writes observed
    /// health here; the reconcile preserves it on unchanged workers.
    pub fn control(&self) -> Arc<dyn WorkerRegistryControl> {
        Arc::new(RegistryStateControl::new(self.state.clone(), self.clock.clone()))
    }
}

impl Drop for MembershipWorkerRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WorkerRegistry for MembershipWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.state.snapshot()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.state.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.state.lookup_by_address(addr)
    }
    fn changes(&self) -> broadcast::Receiver<super::RegistryEvent> {
        self.state.changes()
    }
}

/// A brand-new worker entry for `peer`: address = Pod IP + `sip_port`, health
/// `Unknown` (not routable until the OPTIONS probe confirms it), `first_seen`
/// stamped so the LB fresh-pod guard engages for a just-(re)joined pod.
fn fresh_entry(peer: &Peer, sip_port: u16, now_ms: u64) -> WorkerEntry {
    WorkerEntry {
        id: peer.ordinal.clone(),
        address: ProxyAddr::new(peer.host.clone(), sip_port),
        health: WorkerHealth::Unknown,
        draining_since: None,
        first_seen_at_ms: Some(now_ms),
    }
}

/// Reconcile the worker set to `desired` (the membership snapshot), emitting only
/// the deltas that close the gap and **preserving probe-written health** on
/// unchanged workers (ADR-0012 D4). Pure — the testable heart of the registry: a
/// `RegistryState` + a desired peer set in, the right `RegistryEvent`s out, no
/// cluster needed. Mirrors `topology::reconcile_to_desired` for `WorkerEntry`.
pub(crate) fn reconcile_to_desired(
    state: &RegistryState,
    desired: Vec<Peer>,
    sip_port: u16,
    now_ms: u64,
) {
    // De-dup desired by ordinal (first wins) — two EndpointSlices may list a pod.
    let mut seen = HashSet::new();
    let desired: Vec<Peer> = desired.into_iter().filter(|p| seen.insert(p.ordinal.clone())).collect();
    let desired_ids: HashSet<&str> = desired.iter().map(|p| p.ordinal.as_str()).collect();

    // Removals: a current worker no longer in the membership set.
    for cur in state.snapshot() {
        if !desired_ids.contains(cur.id.as_str()) {
            state.remove_worker(&cur.id);
        }
    }
    // Adds + address changes; an unchanged worker emits nothing and KEEPS its
    // health (the mutators are no-ops when nothing changed).
    for peer in desired {
        match state.resolve(&peer.ordinal) {
            None => state.add_worker(fresh_entry(&peer, sip_port, now_ms)),
            Some(cur) if cur.address.host != peer.host => {
                state.set_address(&peer.ordinal, ProxyAddr::new(peer.host, sip_port), now_ms)
            }
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(items: &[(&str, &str)]) -> Vec<Peer> {
        items.iter().map(|(o, h)| Peer::new(*o, *h)).collect()
    }

    #[test]
    fn new_workers_start_unknown_with_first_seen_stamped() {
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")]), 5060, 1_000);
        let snap = state.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|w| w.health == WorkerHealth::Unknown));
        let w0 = state.resolve("w0").unwrap();
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5060));
        assert_eq!(w0.first_seen_at_ms, Some(1_000));
    }

    #[test]
    fn unchanged_reconcile_preserves_probe_written_health() {
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1")]), 5060, 0);
        // The OPTIONS probe marks it Alive.
        state.set_health("w0", WorkerHealth::Alive, 10);
        // A later no-op reconcile (same set) must NOT stomp the health back to Unknown.
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1")]), 5060, 20);
        assert_eq!(state.resolve("w0").unwrap().health, WorkerHealth::Alive);
    }

    #[test]
    fn departed_worker_is_removed_new_one_added() {
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1")]), 5060, 0);
        state.set_health("w0", WorkerHealth::Alive, 0);
        // w0 leaves, w1 joins.
        reconcile_to_desired(&state, peers(&[("w1", "10.0.0.9")]), 5060, 5);
        assert!(state.resolve("w0").is_none());
        let w1 = state.resolve("w1").unwrap();
        assert_eq!(w1.health, WorkerHealth::Unknown);
        assert_eq!(w1.address, ProxyAddr::new("10.0.0.9", 5060));
    }

    #[test]
    fn address_move_resets_health_and_rearms_fresh_pod_guard() {
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1")]), 5060, 0);
        state.set_health("w0", WorkerHealth::Alive, 0);
        // Same ordinal, new Pod IP (a rare AddressChanged rather than Remove→Add).
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.2")]), 5060, 100);
        let w0 = state.resolve("w0").unwrap();
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.2", 5060));
        assert_eq!(w0.health, WorkerHealth::Unknown, "a fresh endpoint is re-probed");
        assert_eq!(w0.first_seen_at_ms, Some(100), "fresh-pod guard re-armed");
    }

    #[test]
    fn restart_of_dead_worker_resets_and_reprobes() {
        // The live degradation (endurance-20260602-214000): a worker the OPTIONS
        // probe marked `Dead` (the kill) recreates under the same ordinal at a new
        // Pod IP. The reconcile must NOT preserve the stale `Dead` — it must reset
        // to `Unknown` so the probe loop re-probes the new address and can flip it
        // back `Alive`. Discriminator is the address change, not the ordinal.
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w1", "10.0.0.1")]), 5060, 0);
        state.set_health("w1", WorkerHealth::Dead, 50); // killed: probe missed its threshold
        // Same ordinal, recreated at a new Pod IP.
        reconcile_to_desired(&state, peers(&[("w1", "10.0.0.2")]), 5060, 100);
        let w1 = state.resolve("w1").unwrap();
        assert_eq!(w1.address, ProxyAddr::new("10.0.0.2", 5060));
        assert_eq!(w1.health, WorkerHealth::Unknown, "a stale `Dead` must not survive a restart");
        assert_eq!(w1.first_seen_at_ms, Some(100), "fresh-pod guard re-armed for the new incarnation");
    }

    #[test]
    fn dedups_duplicate_ordinals_first_wins() {
        let state = RegistryState::new(vec![]);
        reconcile_to_desired(&state, peers(&[("w0", "10.0.0.1"), ("w0", "10.0.0.2")]), 5060, 0);
        assert_eq!(state.resolve("w0").unwrap().address, ProxyAddr::new("10.0.0.1", 5060));
    }
}
