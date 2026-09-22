//! Peer-to-peer call replication a runner composes from env
//! ([`ReplicationSettings`]): where the peer set comes from, how a peer's
//! replication address is resolved, and the [`ReplicationSetup`] a runner
//! assigns to `deps.replication`. Replication off leaves the node unwired.
//!
//! ## Env grammar
//!   B2BUA_REPL          truthy (`1`/`true`/`yes`/`on`) enables replication (default off)
//!   B2BUA_REPL_LISTEN   replication TCP listen addr        (default 0.0.0.0:9092)
//!   B2BUA_REPL_PORT     port every peer is reached on      (default = the REPL_LISTEN port)
//!   B2BUA_PEERS         static membership `ord@host,..`; set, it takes precedence
//!                       over discovery; malformed, replication stays off
//!   B2BUA_REPL_SERVICE  headless Service whose EndpointSlices name the peers
//!                       (default b2bua-worker)
//!   B2BUA_NAMESPACE     namespace of that Service          (default $POD_NAMESPACE, then sip-test)
//!
//! A malformed `B2BUA_REPL_LISTEN` or `B2BUA_REPL_PORT` refuses the boot. A
//! malformed peer list, or discovery without an in-cluster kube client, leaves
//! the node unwired and logs why: the worker still serves SIP.
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

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use b2bua::repl::{PeerResolver, ReplicatingCallStore};
use b2bua::ReplicationSetup;
use repl_net::RealReplicationNetwork;
use sip_clock::Clock;
use topology::{parse_peer_list, Membership, Peer, StaticMembership};

use crate::is_truthy;

/// Where the peer set comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipSource {
    /// `B2BUA_PEERS`, parsed, sorted by ordinal.
    Static(Vec<Peer>),
    /// The EndpointSlices of the headless `service` in `namespace`, watched
    /// through the in-cluster kube client.
    EndpointSlices { service: String, namespace: String },
}

/// How a peer's replication address is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplAddressing {
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
    pub listen: SocketAddr,
    /// The port every peer is reached on, `B2BUA_REPL_PORT`.
    pub peer_port: u16,
    /// `B2BUA_PEERS` when set, else discovery of `B2BUA_REPL_SERVICE`.
    pub membership: MembershipSource,
}

impl ReplicationSettings {
    /// Reads the grammar through `get`. `Ok(None)` when replication is off or
    /// the peer list is malformed (logged); `Err` naming the variable when the
    /// listen address or the peer port is malformed.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        if !get("B2BUA_REPL").is_some_and(|v| is_truthy(&v)) {
            return Ok(None);
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
            match parse_peer_list("B2BUA_PEERS", &peers) {
                Ok(mut list) => {
                    list.sort_by(|a, b| a.ordinal.cmp(&b.ordinal));
                    MembershipSource::Static(list)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "B2BUA_PEERS parse error — replication disabled");
                    return Ok(None);
                }
            }
        };
        Ok(Some(Self { listen, peer_port, membership }))
    }

    /// How the membership these settings select is addressed.
    pub fn addressing(&self) -> ReplAddressing {
        match &self.membership {
            MembershipSource::Static(_) => ReplAddressing::Static,
            MembershipSource::EndpointSlices { service, namespace } => {
                ReplAddressing::PodDns { service: service.clone(), namespace: namespace.clone() }
            }
        }
    }

    /// The setup these settings describe: the membership, a replicating store
    /// under `incarnation_gen` on `clock`, the real replication transport and
    /// the per-attempt peer resolver. `None` (logged) when discovery has no
    /// in-cluster kube client.
    pub async fn into_setup(self, clock: Clock, incarnation_gen: u64) -> Option<ReplicationSetup> {
        let addressing = self.addressing();
        let membership = self.membership.into_membership().await?;
        tracing::info!(
            listen = %self.listen,
            peer_port = self.peer_port,
            incarnation_gen,
            "replication ENABLED"
        );
        spawn_membership_log(membership.clone());
        Some(ReplicationSetup {
            network: Arc::new(RealReplicationNetwork::new()),
            membership,
            store: Arc::new(ReplicatingCallStore::new(incarnation_gen, clock)),
            listen_addr: self.listen,
            addr_resolver: Arc::new(ReplResolver { repl_port: self.peer_port, addressing }),
            incarnation_gen,
        })
    }
}

impl MembershipSource {
    async fn into_membership(self) -> Option<Arc<dyn Membership>> {
        match self {
            MembershipSource::Static(peers) => {
                let listed: Vec<String> =
                    peers.iter().map(|p| format!("{}@{}", p.ordinal, p.host)).collect();
                tracing::info!(
                    source = "B2BUA_PEERS",
                    peers = %listed.join(","),
                    "replication membership"
                );
                Some(Arc::new(StaticMembership::from_peers(peers)))
            }
            MembershipSource::EndpointSlices { service, namespace } => {
                // Installing the provider a second time returns Err; ignored.
                let _ = rustls::crypto::ring::default_provider().install_default();
                match kube::Client::try_default().await {
                    Ok(client) => {
                        tracing::info!(
                            source = "k8s-endpointslice",
                            %service,
                            %namespace,
                            "replication membership"
                        );
                        Some(Arc::new(topology::K8sMembership::spawn(client, namespace, service)))
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "no kube client and no B2BUA_PEERS — replication disabled"
                        );
                        None
                    }
                }
            }
        }
    }
}

/// The stable DNS name of a discovered peer: its ordinal is its pod name
/// (the EndpointSlice `targetRef`), under the headless `service`.
pub fn pod_dns_name(ordinal: &str, service: &str, namespace: &str) -> String {
    format!("{ordinal}.{service}.{namespace}.svc.cluster.local")
}

/// The replication setup the grammar read through `get` selects, its store on
/// `clock` under a boot-wall-clock incarnation gen; `Ok(None)` when
/// replication is off or has no membership.
pub async fn replication_setup_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    clock: Clock,
) -> Result<Option<ReplicationSetup>, String> {
    let Some(settings) = ReplicationSettings::from_lookup(get)? else {
        return Ok(None);
    };
    Ok(settings.into_setup(clock, boot_incarnation()).await)
}

/// Boot wall clock in milliseconds, so a sub-second restart still takes a
/// higher gen; 0 only for a clock before the epoch.
fn boot_incarnation() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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

/// Resolves a peer to `<address>:repl_port` afresh on every connect attempt;
/// `None` makes the puller back off and retry.
struct ReplResolver {
    repl_port: u16,
    addressing: ReplAddressing,
}

#[async_trait]
impl PeerResolver for ReplResolver {
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr> {
        let addr = match &self.addressing {
            ReplAddressing::Static => match peer.host.parse::<IpAddr>() {
                Ok(ip) => Some(SocketAddr::new(ip, self.repl_port)),
                Err(_) => lookup_host(&peer.host, self.repl_port).await,
            },
            ReplAddressing::PodDns { service, namespace } => {
                let name = pod_dns_name(&peer.ordinal, service, namespace);
                match lookup_host(&name, self.repl_port).await {
                    Some(a) => Some(a),
                    None => peer
                        .host
                        .parse::<IpAddr>()
                        .ok()
                        .map(|ip| SocketAddr::new(ip, self.repl_port)),
                }
            }
        };
        // One line per attempt: a new address after a restart shows the
        // puller followed the peer.
        match addr {
            Some(a) => tracing::info!(peer = %peer.ordinal, addr = %a, "repl peer resolved"),
            None => tracing::warn!(peer = %peer.ordinal, "repl peer unresolvable (will retry)"),
        }
        addr
    }
}

async fn lookup_host(host: &str, port: u16) -> Option<SocketAddr> {
    tokio::net::lookup_host((host, port)).await.ok().and_then(|mut a| a.next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn settings(pairs: &[(&str, &str)]) -> ReplicationSettings {
        ReplicationSettings::from_lookup(lookup(pairs)).expect("ok").expect("replication on")
    }

    #[test]
    fn replication_off_selects_no_setup() {
        assert_eq!(ReplicationSettings::from_lookup(lookup(&[])), Ok(None));
        assert_eq!(
            ReplicationSettings::from_lookup(lookup(&[
                ("B2BUA_REPL", "0"),
                ("B2BUA_PEERS", "w0@10.0.0.1"),
            ])),
            Ok(None),
            "a peer list alone does not turn replication on"
        );
    }

    #[test]
    fn a_malformed_peer_list_leaves_replication_off() {
        for peers in ["noatsign", "w0@10.0.0.1,w0@10.0.0.2", "w0@10.0.0.1,,w1@10.0.0.2"] {
            assert_eq!(
                ReplicationSettings::from_lookup(lookup(&[
                    ("B2BUA_REPL", "1"),
                    ("B2BUA_PEERS", peers),
                ])),
                Ok(None),
                "{peers:?} must leave the node unwired, not refuse the boot"
            );
        }
    }

    #[test]
    fn a_static_peer_list_selects_static_addressing() {
        let s = settings(&[("B2BUA_REPL", "true"), ("B2BUA_PEERS", "w1@10.0.0.2, w0@10.0.0.1")]);
        assert_eq!(
            s,
            ReplicationSettings {
                listen: SocketAddr::from(([0, 0, 0, 0], 9092)),
                peer_port: 9092,
                membership: MembershipSource::Static(vec![
                    Peer::new("w0", "10.0.0.1"),
                    Peer::new("w1", "10.0.0.2"),
                ]),
            }
        );
        assert_eq!(s.addressing(), ReplAddressing::Static);
    }

    #[test]
    fn the_peer_port_defaults_to_the_listen_port_and_is_read_from_its_variable() {
        let peers = ("B2BUA_PEERS", "w0@10.0.0.1");
        let on = ("B2BUA_REPL", "1");
        let own = settings(&[on, peers, ("B2BUA_REPL_LISTEN", "127.0.0.1:9200")]);
        assert_eq!((own.listen, own.peer_port), (SocketAddr::from(([127, 0, 0, 1], 9200)), 9200));
        let cluster = settings(&[on, peers, ("B2BUA_REPL_PORT", "9100")]);
        assert_eq!(cluster.peer_port, 9100);
    }

    #[test]
    fn a_malformed_listen_address_or_peer_port_is_refused_naming_its_variable() {
        let on = ("B2BUA_REPL", "1");
        let e = ReplicationSettings::from_lookup(lookup(&[on, ("B2BUA_REPL_PORT", "nine")]))
            .expect_err("a non-numeric port must refuse boot");
        assert!(e.contains("B2BUA_REPL_PORT"), "msg was: {e}");
        let e = ReplicationSettings::from_lookup(lookup(&[on, ("B2BUA_REPL_LISTEN", "nowhere")]))
            .expect_err("a listen address with no port must refuse boot");
        assert!(e.contains("B2BUA_REPL_LISTEN"), "msg was: {e}");
    }

    #[test]
    fn no_peer_list_discovers_the_peers_from_the_service_endpoint_slices() {
        let on = ("B2BUA_REPL", "1");
        let s = settings(&[on]);
        assert_eq!(
            s.membership,
            MembershipSource::EndpointSlices {
                service: "b2bua-worker".into(),
                namespace: "sip-test".into(),
            }
        );
        assert_eq!(
            s.addressing(),
            ReplAddressing::PodDns { service: "b2bua-worker".into(), namespace: "sip-test".into() }
        );
        let pod_ns = settings(&[on, ("POD_NAMESPACE", "lab"), ("B2BUA_REPL_SERVICE", "workers")]);
        assert_eq!(
            pod_ns.membership,
            MembershipSource::EndpointSlices { service: "workers".into(), namespace: "lab".into() }
        );
        let explicit = settings(&[on, ("POD_NAMESPACE", "lab"), ("B2BUA_NAMESPACE", "prod")]);
        assert!(
            matches!(&explicit.membership, MembershipSource::EndpointSlices { namespace, .. } if namespace == "prod"),
            "B2BUA_NAMESPACE wins over POD_NAMESPACE: {:?}",
            explicit.membership
        );
    }

    /// A discovered peer's ordinal is its pod name, so the stable name is the
    /// pod name under the headless Service, not a prefix added to it.
    #[test]
    fn a_discovered_peer_is_named_by_its_pod_name_under_the_service() {
        assert_eq!(
            pod_dns_name("b2bua-worker-0", "b2bua-worker", "prod"),
            "b2bua-worker-0.b2bua-worker.prod.svc.cluster.local"
        );
    }

    #[tokio::test]
    async fn a_static_setup_reaches_each_peer_on_the_cluster_replication_port() {
        let setup = replication_setup_from_lookup(
            lookup(&[
                ("B2BUA_REPL", "1"),
                ("B2BUA_PEERS", "w0@10.0.0.1,w1@10.0.0.2"),
                ("B2BUA_REPL_LISTEN", "127.0.0.1:9200"),
                ("B2BUA_REPL_PORT", "9100"),
            ]),
            Clock::system(),
        )
        .await
        .expect("ok")
        .expect("a static peer list needs no cluster to wire replication");
        assert_eq!(setup.listen_addr, SocketAddr::from(([127, 0, 0, 1], 9200)));
        assert_eq!(
            setup.membership.snapshot(),
            vec![Peer::new("w0", "10.0.0.1"), Peer::new("w1", "10.0.0.2")]
        );
        assert_eq!(
            setup.addr_resolver.resolve(&Peer::new("w1", "10.0.0.2")).await,
            Some(SocketAddr::from(([10, 0, 0, 2], 9100)))
        );
        assert!(setup.incarnation_gen > 0, "the gen is the boot wall clock");
    }
}
