//! Peer-to-peer call replication a runner composes from env
//! ([`ReplicationSettings`]): where the peer set comes from, how a peer's
//! replication address is resolved, and the [`ReplicationSetup`] a runner
//! assigns to `deps.replication`. Replication off leaves the node unwired;
//! replication on never runs without it.
//!
//! ## Env grammar
//!   B2BUA_REPL          truthy (`1`/`true`/`yes`/`on`) enables replication (default off)
//!   B2BUA_REPL_LISTEN   replication TCP listen addr        (default 0.0.0.0:9092)
//!   B2BUA_REPL_PORT     port every peer is reached on      (default = the REPL_LISTEN port)
//!   B2BUA_PEERS         static membership `ord@host,..`; set, it takes precedence
//!                       over discovery
//!   B2BUA_REPL_SERVICE  headless Service whose EndpointSlices name the peers
//!                       (default b2bua-worker)
//!   B2BUA_NAMESPACE     namespace of that Service          (default $POD_NAMESPACE, then sip-test)
//!   B2BUA_REPL_*_MS …   the link bounds ([`link_env`], ADR-0045)
//!
//! A malformed `B2BUA_REPL_LISTEN`, `B2BUA_REPL_PORT`, `B2BUA_PEERS` or link
//! bound, or a listen address that cannot be bound, refuses the boot. A kube
//! client that cannot be built is built again in the background: the node
//! stays wired, its view empty and unsynced until the informer lists.
//!
//! ## Addressing (ADR-0012 D3)
//! Membership is port-agnostic; every peer's replication server is at
//! `<host>:B2BUA_REPL_PORT`, one cluster-wide port. The address is resolved on
//! every connect attempt, so a restarted peer's new IP is picked up without a
//! membership change. A static peer's host is used as given (an IP, or a name
//! resolved per attempt). A discovered peer is reached by its stable pod DNS
//! name ([`pod_dns_name`]), falling back to its EndpointSlice address (the pod
//! IP) when DNS misses.
//!
//! ## Incarnation
//! The changelog's incarnation gen is the boot wall clock in milliseconds, so
//! a restarted node serves under a higher gen than its previous life
//! (ADR-0011 X9); a backward clock step is caught by `Changelog::needs_reset`.

use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use b2bua::repl::ReplicatingCallStore;
use b2bua::ReplicationSetup;
use http_net::SystemResolver;
use repl_net::{RealReplicationNetwork, ReplicationNetwork};
use sip_clock::Clock;
use topology::{parse_peer_list, Membership, Peer, StaticMembership};

use crate::is_truthy;

mod link_env;
mod repl_resolver;
mod write_path_env;
use link_env::LinkSettings;
use repl_resolver::ReplResolver;

/// Where the peer set comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MembershipSource {
    /// `B2BUA_PEERS`, parsed, sorted by ordinal.
    Static(Vec<Peer>),
    /// The EndpointSlices of the headless `service` in `namespace`, watched
    /// through the in-cluster kube client.
    EndpointSlices { service: String, namespace: String },
}

/// How a peer's replication address is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplAddressing {
    /// The peer's host as given: an IP, or a name resolved per connect attempt.
    Static,
    /// The peer's stable pod DNS name ([`pod_dns_name`]), falling back to its
    /// host (the pod IP) when DNS misses.
    PodDns { service: String, namespace: String },
}

/// The replication env grammar, the one statement of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationSettings {
    /// Where this node serves its changelog, `B2BUA_REPL_LISTEN`.
    pub(crate) listen: SocketAddr,
    /// The port every peer is reached on, `B2BUA_REPL_PORT`.
    pub(crate) peer_port: u16,
    /// `B2BUA_PEERS` when set, else discovery of `B2BUA_REPL_SERVICE`.
    pub(crate) membership: MembershipSource,
    /// The link bounds, `B2BUA_REPL_*_MS` ([`link_env`]).
    pub(crate) link: LinkSettings,
    /// The hold cap and backlog marks ([`write_path_env`]).
    pub(crate) write_path: b2bua::repl::WritePath,
}

impl ReplicationSettings {
    /// Reads the grammar through `get`. `Ok(None)` when replication is off;
    /// `Err` naming the variable when one is malformed, or when replication is
    /// on with no `B2BUA_CLUSTER_KEY`: the owners of a call trust what each
    /// states on its requests only under that key (ADR-0055 §12).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        if !get("B2BUA_REPL").is_some_and(|v| is_truthy(&v)) {
            return Ok(None);
        }
        if get("B2BUA_CLUSTER_KEY").is_none_or(|k| k.trim().is_empty()) {
            return Err("B2BUA_REPL is on but B2BUA_CLUSTER_KEY is unset: the owners of a call \
                 sign and verify the via-extension with it (ADR-0055 §12)"
                .to_string());
        }
        let raw_listen = get("B2BUA_REPL_LISTEN").unwrap_or_else(|| "0.0.0.0:9092".to_string());
        let listen =
            raw_listen.to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| {
                format!("B2BUA_REPL_LISTEN must be a resolvable host:port, got {raw_listen:?}")
            })?;
        let peer_port = match get("B2BUA_REPL_PORT") {
            None => listen.port(),
            Some(raw) => raw
                .parse()
                .map_err(|e| format!("B2BUA_REPL_PORT must be a port number, got {raw:?}: {e}"))?,
        };
        let peers = get("B2BUA_PEERS").unwrap_or_default();
        let membership = if peers.trim().is_empty() {
            MembershipSource::EndpointSlices {
                service: get("B2BUA_REPL_SERVICE").unwrap_or_else(|| "b2bua-worker".to_string()),
                namespace: get("B2BUA_NAMESPACE")
                    .or_else(|| get("POD_NAMESPACE"))
                    .unwrap_or_else(|| "sip-test".to_string()),
            }
        } else {
            let mut list = parse_peer_list("B2BUA_PEERS", &peers).map_err(|e| e.to_string())?;
            list.sort_by(|a, b| a.ordinal.cmp(&b.ordinal));
            MembershipSource::Static(list)
        };
        let link = LinkSettings::from_lookup(&get)?;
        let write_path = write_path_env::write_path(&get)?;
        Ok(Some(Self { listen, peer_port, membership, link, write_path }))
    }

    /// How the membership these settings select is addressed.
    pub(crate) fn addressing(&self) -> ReplAddressing {
        match &self.membership {
            MembershipSource::Static(_) => ReplAddressing::Static,
            MembershipSource::EndpointSlices { service, namespace } => {
                ReplAddressing::PodDns { service: service.clone(), namespace: namespace.clone() }
            }
        }
    }

    /// The setup these settings describe: the membership, a replicating store
    /// under `incarnation_gen` on `clock`, the real replication transport, the
    /// bound listener and the per-attempt peer resolver. `Err` naming
    /// `B2BUA_REPL_LISTEN` when the listener cannot be bound.
    pub async fn into_setup(
        self,
        clock: Clock,
        incarnation_gen: u64,
    ) -> Result<ReplicationSetup, String> {
        let addressing = self.addressing();
        let network = RealReplicationNetwork::with_socket(self.link.socket);
        let listener = network
            .listen(self.listen)
            .await
            .map_err(|e| format!("B2BUA_REPL_LISTEN {} cannot be bound: {e}", self.listen))?;
        let membership = self.membership.into_membership();
        tracing::info!(
            listen = %self.listen,
            peer_port = self.peer_port,
            incarnation_gen,
            "replication ENABLED"
        );
        spawn_membership_log(membership.clone());
        Ok(ReplicationSetup {
            network: Arc::new(network),
            membership,
            store: Arc::new(ReplicatingCallStore::new(incarnation_gen, clock)),
            listener,
            addr_resolver: Arc::new(ReplResolver::new(
                self.peer_port,
                addressing,
                Arc::new(SystemResolver),
                self.link.lookup_timeout,
            )),
            incarnation_gen,
            puller: self.link.puller(),
            serve: self.link.serve(),
            replica: b2bua::ReplicaConfig::default(),
            write_path: self.write_path,
        })
    }
}

impl MembershipSource {
    fn into_membership(self) -> Arc<dyn Membership> {
        self.into_membership_with(|| async {
            kube::Client::try_default().await.map_err(|e| e.to_string())
        })
    }

    /// The membership, a discovered one watching through the kube client
    /// `connect` builds, built again with backoff until it is.
    pub(crate) fn into_membership_with<F, Fut>(self, connect: F) -> Arc<dyn Membership>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<kube::Client, String>> + Send,
    {
        match self {
            MembershipSource::Static(peers) => {
                let listed: Vec<String> =
                    peers.iter().map(|p| format!("{}@{}", p.ordinal, p.host)).collect();
                tracing::info!(
                    source = "B2BUA_PEERS",
                    peers = %listed.join(","),
                    "replication membership"
                );
                Arc::new(StaticMembership::from_peers(peers))
            }
            MembershipSource::EndpointSlices { service, namespace } => {
                // Installing the provider a second time returns Err; ignored.
                let _ = rustls::crypto::ring::default_provider().install_default();
                tracing::info!(
                    source = "k8s-endpointslice",
                    %service,
                    %namespace,
                    "replication membership"
                );
                let (restarts, task) =
                    (&b2bua::supervise::RESTARTS, b2bua::supervise::K8S_MEMBERSHIP);
                Arc::new(topology::K8sMembership::spawn_connecting(
                    connect, namespace, service, restarts, task,
                ))
            }
        }
    }
}

/// The stable DNS name of a discovered peer: its ordinal is its pod name
/// (the EndpointSlice `targetRef`), under the headless `service`.
pub(crate) fn pod_dns_name(ordinal: &str, service: &str, namespace: &str) -> String {
    format!("{ordinal}.{service}.{namespace}.svc.cluster.local")
}

/// The replication setup the grammar read through `get` selects, its store on
/// `clock` under a boot-wall-clock incarnation gen; `Ok(None)` when
/// replication is off, `Err` when it cannot run as configured.
pub async fn replication_setup_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    clock: Clock,
) -> Result<Option<ReplicationSetup>, String> {
    let Some(settings) = ReplicationSettings::from_lookup(get)? else {
        return Ok(None);
    };
    settings.into_setup(clock, boot_incarnation()).await.map(Some)
}

/// Boot wall clock in milliseconds, so a sub-second restart still takes a
/// higher gen; 0 only for a clock before the epoch.
fn boot_incarnation() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(sip_clock::wall::millis_u64).unwrap_or(0)
}

/// Logs the peer set every 2 s for 12 s after boot: a discovered membership
/// starts empty and fills asynchronously, so a set still empty then names a
/// discovery problem rather than a replication one.
fn spawn_membership_log(membership: Arc<dyn Membership>) {
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let peers: Vec<String> = membership
                .snapshot()
                .into_iter()
                .map(|p| {
                    format!(
                        "{}@{} ready={} terminating={}",
                        p.ordinal, p.host, p.ready, p.terminating
                    )
                })
                .collect();
            tracing::info!(peers = %peers.join(", "), "repl membership snapshot");
        }
    });
}

#[cfg(test)]
mod tests;
