//! Peer-to-peer call replication a runner composes from env
//! ([`ReplicationSettings`]). Stub: the grammar is not read yet.

use std::net::SocketAddr;

use b2bua::ReplicationSetup;
use sip_clock::Clock;
use topology::Peer;

/// Where the peer set comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipSource {
    /// `B2BUA_PEERS`, parsed.
    Static(Vec<Peer>),
    /// The EndpointSlices of the headless `service` in `namespace`.
    EndpointSlices { service: String, namespace: String },
}

/// How a peer's replication address is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplAddressing {
    /// The peer's host as given.
    Static,
    /// The peer's stable pod DNS name.
    PodDns { service: String, namespace: String },
}

/// The replication env grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationSettings {
    /// `B2BUA_REPL_LISTEN`.
    pub listen: SocketAddr,
    /// `B2BUA_REPL_PORT`.
    pub peer_port: u16,
    /// `B2BUA_PEERS`, or discovery.
    pub membership: MembershipSource,
}

impl ReplicationSettings {
    /// Stub.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let _ = get;
        Ok(None)
    }

    /// Stub.
    pub fn addressing(&self) -> ReplAddressing {
        ReplAddressing::Static
    }
}

/// Stub.
pub fn pod_dns_name(ordinal: &str, service: &str, namespace: &str) -> String {
    let _ = (ordinal, service, namespace);
    String::new()
}

/// Stub.
pub async fn replication_setup_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    clock: Clock,
) -> Result<Option<ReplicationSetup>, String> {
    let _ = (get, clock);
    Ok(None)
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
