//! `K8sMembership` — the real cluster-membership source (slice S11), backed by a
//! Kubernetes **EndpointSlice informer**. Behind the `kube` feature so the
//! fake-clock test tiers and the proxy/b2bua *libraries* never compile the kube
//! client stack; only the `b2bua-runner` (and a future proxy runner) opts in.
//!
//! ADR-0011 X7: the k8s watcher is written **once**, here, and consumed by both
//! the proxy and the b2bua replication engine — neither re-implements discovery.
//! It watches the EndpointSlices of one headless Service (the worker pool) and
//! translates **every** endpoint in them into the same [`Peer`]/[`MemberDelta`]
//! stream the [`SimulatedMembership`](crate::SimulatedMembership) and
//! [`StaticMembership`](crate::StaticMembership) produce, each `Peer` carrying
//! the endpoint's `ready` and `terminating` conditions. Each consumer applies its
//! own predicate (ADR-0031 D1): the proxy routes to `ready` members, the
//! replication supervisor pulls every member present. A condition flip is a
//! `ConditionsChanged` delta, never a departure or a host move.
//!
//! ## Why EndpointSlices (not Pods)
//! EndpointSlices already encode the endpoint *conditions* (`ready`, `serving`,
//! `terminating`; the last two need Kubernetes ≥ 1.26) and a stable
//! `targetRef` back to the owning Pod, and they are the canonical, watch-cheap
//! source the kube-proxy itself consumes. The `ordinal` we expose is the Pod
//! name (`targetRef.name`) — which for a StatefulSet is the stable replica
//! identity (`b2bua-worker-0`) the runner also stamps into `B2BUA_ORDINAL`, so
//! membership ordinals and callRef ordinals agree by construction. The `host`
//! is the endpoint's first address (the Pod IP) — **port-agnostic**, as
//! membership must be (the repl port is layered on by the consumer).

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client};
use tokio::sync::broadcast;

use crate::{reconcile_to_desired, MemberDelta, Membership, MembershipState, Peer};

/// The standard label kube stamps on every EndpointSlice pointing back to its
/// owning Service. Watching by this selector scopes the informer to one pool.
const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// Membership derived from a live EndpointSlice watch. Cheap to share via the
/// `Membership` trait object; the background informer task is aborted on drop.
pub struct K8sMembership {
    state: Arc<MembershipState>,
    /// Flipped once the informer's initial LIST completes (`Event::InitDone`).
    /// Until then the snapshot is the constructor's empty placeholder, NOT an
    /// authoritative "this cluster has no peers" — see [`Membership::synced`].
    synced: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl K8sMembership {
    /// Start watching the EndpointSlices of `service_name` in `namespace` and
    /// drive a [`MembershipState`] from them. Returns immediately with an empty
    /// snapshot; the informer fills it (and emits deltas) as the watch streams
    /// in. **Best-effort**: a watch error is logged and retried by the kube
    /// runtime — it never tears the process down (liveness over completeness,
    /// ADR-0011 X5), matching the b2bua's "boot and serve even if peers are
    /// unreachable" stance.
    pub fn spawn(
        client: Client,
        namespace: impl Into<String>,
        service_name: impl Into<String>,
    ) -> Self {
        let namespace = namespace.into();
        let service_name = service_name.into();
        let state = Arc::new(MembershipState::new(vec![]));

        let api: Api<EndpointSlice> = Api::namespaced(client, &namespace);
        let cfg =
            watcher::Config::default().labels(&format!("{SERVICE_NAME_LABEL}={service_name}"));
        let (reader, writer) = reflector::store();

        let synced = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let st = state.clone();
        let synced_flag = synced.clone();
        let task = tokio::spawn(async move {
            // `reflector` keeps `reader` an up-to-date cache of the whole slice
            // set; we recompute the desired peer set from that full cache on
            // every event, so partial/restart watches converge rather than
            // emit spurious churn. `default_backoff` makes the watch self-heal.
            let stream = reflector(writer, watcher(api, cfg)).default_backoff();
            futures::pin_mut!(stream);
            loop {
                match stream.next().await {
                    Some(Ok(event)) => {
                        let slices = reader.state();
                        let desired = peers_from_slices(slices.iter().map(|s| s.as_ref()));
                        reconcile_to_desired(&st, desired);
                        // Initial LIST complete: the snapshot is now authoritative
                        // (an empty set really means a peerless pool). Sticky —
                        // re-list churn after a watch error never un-syncs.
                        if matches!(event, watcher::Event::InitDone) {
                            synced_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "EndpointSlice watch error (will retry)");
                    }
                    // The watcher stream is infinite; `None` only on shutdown.
                    None => break,
                }
            }
        });

        Self { state, synced, task }
    }
}

impl Drop for K8sMembership {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Membership for K8sMembership {
    fn snapshot(&self) -> Vec<Peer> {
        self.state.snapshot()
    }
    fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        self.state.changes()
    }
    fn synced(&self) -> bool {
        self.synced.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// The slice set of the pool holds the watching worker's own endpoint.
    fn observes_self(&self) -> bool {
        true
    }
}

/// Translate a set of EndpointSlices into the desired [`Peer`] set: one peer per
/// Pod **present** in any slice, whatever its conditions, identified by
/// `targetRef.name` (falling back to `hostname`) at its first address.
/// `ready` is the explicit `conditions.ready` (absent reads as not ready);
/// `terminating` is `conditions.terminating` (absent reads as false). `serving`
/// is not carried: pullable is keyed on presence, routable on `ready`
/// (ADR-0031 D1). A Pod listed in two slices (a slice rebalance in flight) is one
/// peer: the conditions are OR-ed across its copies and the first host seen
/// wins, so a worker that is ready in either slice is ready — slice order is
/// arbitrary. Endpoints with no address or no identity are skipped. Pure — the
/// testable heart of the informer (a synthetic slice in, the expected peers out;
/// no cluster needed).
fn peers_from_slices<'a>(slices: impl IntoIterator<Item = &'a EndpointSlice>) -> Vec<Peer> {
    let mut out: Vec<Peer> = Vec::new();
    for slice in slices {
        for ep in &slice.endpoints {
            let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(false);
            let terminating = ep.conditions.as_ref().and_then(|c| c.terminating).unwrap_or(false);
            let Some(host) = ep.addresses.first().cloned() else {
                continue;
            };
            let ordinal =
                ep.target_ref.as_ref().and_then(|r| r.name.clone()).or_else(|| ep.hostname.clone());
            let Some(ordinal) = ordinal else {
                continue;
            };
            match out.iter_mut().find(|p| p.ordinal == ordinal) {
                Some(seen) => {
                    seen.ready |= ready;
                    seen.terminating |= terminating;
                }
                None => out.push(Peer::with_conditions(ordinal, host, ready, terminating)),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::ObjectReference;
    use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions};

    fn endpoint(pod: &str, ip: &str, ready: Option<bool>) -> Endpoint {
        endpoint_with(pod, ip, ready, None)
    }

    fn endpoint_with(
        pod: &str,
        ip: &str,
        ready: Option<bool>,
        terminating: Option<bool>,
    ) -> Endpoint {
        Endpoint {
            addresses: vec![ip.to_string()],
            conditions: Some(EndpointConditions { ready, terminating, ..Default::default() }),
            target_ref: Some(ObjectReference {
                name: Some(pod.to_string()),
                kind: Some("Pod".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn slice(endpoints: Vec<Endpoint>) -> EndpointSlice {
        EndpointSlice { address_type: "IPv4".to_string(), endpoints, ..Default::default() }
    }

    #[test]
    fn ready_endpoints_become_peers_keyed_by_pod_name() {
        let s = slice(vec![
            endpoint("b2bua-worker-0", "10.0.0.1", Some(true)),
            endpoint("b2bua-worker-1", "10.0.0.2", Some(true)),
        ]);
        let peers = peers_from_slices([&s]);
        assert_eq!(
            peers,
            vec![Peer::new("b2bua-worker-0", "10.0.0.1"), Peer::new("b2bua-worker-1", "10.0.0.2"),]
        );
    }

    #[test]
    fn not_ready_and_unknown_ready_endpoints_are_emitted_as_not_ready() {
        let s = slice(vec![
            endpoint("b2bua-worker-0", "10.0.0.1", Some(true)),
            endpoint("b2bua-worker-1", "10.0.0.2", Some(false)),
            endpoint("b2bua-worker-2", "10.0.0.3", None),
        ]);
        assert_eq!(
            peers_from_slices([&s]),
            vec![
                Peer::new("b2bua-worker-0", "10.0.0.1"),
                Peer::new("b2bua-worker-1", "10.0.0.2").not_ready(),
                Peer::new("b2bua-worker-2", "10.0.0.3").not_ready(),
            ],
            "every member present is a peer; only a ready one is routable"
        );
    }

    #[test]
    fn a_terminating_endpoint_stays_a_peer_with_the_flag() {
        let s = slice(vec![endpoint_with("b2bua-worker-1", "10.0.0.2", Some(false), Some(true))]);
        assert_eq!(
            peers_from_slices([&s]),
            vec![Peer::with_conditions("b2bua-worker-1", "10.0.0.2", false, true)]
        );
        // An absent `terminating` condition reads as false.
        let s = slice(vec![endpoint_with("b2bua-worker-1", "10.0.0.2", Some(true), None)]);
        assert!(!peers_from_slices([&s])[0].terminating);
    }

    #[test]
    fn an_endpoint_without_an_address_is_skipped() {
        let mut ep = endpoint("b2bua-worker-3", "10.0.0.4", Some(true));
        ep.addresses.clear();
        assert!(peers_from_slices([&slice(vec![ep])]).is_empty());
    }

    #[test]
    fn endpoints_across_multiple_slices_are_unioned() {
        let a = slice(vec![endpoint("b2bua-worker-0", "10.0.0.1", Some(true))]);
        let b = slice(vec![endpoint("b2bua-worker-1", "10.0.0.2", Some(true))]);
        let peers = peers_from_slices([&a, &b]);
        assert_eq!(
            peers,
            vec![Peer::new("b2bua-worker-0", "10.0.0.1"), Peer::new("b2bua-worker-1", "10.0.0.2"),]
        );
    }

    #[test]
    fn a_pod_listed_in_two_slices_is_ready_if_either_copy_is() {
        // A slice rebalance lists the same Pod twice for a moment; the informer's
        // cache iterates slices in arbitrary order, so both orders must agree.
        let ready = slice(vec![endpoint("b2bua-worker-0", "10.0.0.1", Some(true))]);
        let stale =
            slice(vec![endpoint_with("b2bua-worker-0", "10.0.0.1", Some(false), Some(true))]);
        let expected = vec![Peer::with_conditions("b2bua-worker-0", "10.0.0.1", true, true)];
        assert_eq!(peers_from_slices([&ready, &stale]), expected, "ready copy first");
        assert_eq!(peers_from_slices([&stale, &ready]), expected, "stale copy first");
    }

    #[test]
    fn endpoint_without_target_ref_falls_back_to_hostname() {
        let mut ep = endpoint("ignored", "10.0.0.9", Some(true));
        ep.target_ref = None;
        ep.hostname = Some("b2bua-worker-9".to_string());
        let s = slice(vec![ep]);
        assert_eq!(peers_from_slices([&s]), vec![Peer::new("b2bua-worker-9", "10.0.0.9")]);
    }
}
