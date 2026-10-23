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
fn a_malformed_peer_list_refuses_boot_naming_its_variable() {
    for peers in ["noatsign", "w0@10.0.0.1,w0@10.0.0.2", "w0@10.0.0.1,,w1@10.0.0.2"] {
        let e = ReplicationSettings::from_lookup(lookup(&[
            ("B2BUA_REPL", "1"),
            ("B2BUA_PEERS", peers),
        ]))
        .expect_err("a malformed peer list must refuse the boot, never run without HA");
        assert!(e.contains("B2BUA_PEERS"), "{peers:?}: msg was: {e}");
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
            link: LinkSettings::default(),
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
async fn a_listen_address_that_cannot_be_bound_refuses_boot_naming_its_variable() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port to hold");
    let listen = taken.local_addr().unwrap().to_string();
    let e = replication_setup_from_lookup(
        lookup(&[
            ("B2BUA_REPL", "1"),
            ("B2BUA_PEERS", "w0@10.0.0.1"),
            ("B2BUA_REPL_LISTEN", &listen),
        ]),
        Clock::system(),
    )
    .await
    .err()
    .expect("a replication listener that cannot bind must refuse the boot");
    assert!(e.contains("B2BUA_REPL_LISTEN"), "msg was: {e}");
    drop(taken);
}

/// A discovered membership whose kube client cannot be built keeps the
/// node wired: the client is built again with backoff, and the view stays
/// unsynced and empty meanwhile.
#[tokio::test(start_paused = true)]
async fn a_kube_client_that_fails_at_boot_is_retried_and_the_node_stays_wired() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let source =
        MembershipSource::EndpointSlices { service: "workers".into(), namespace: "lab".into() };
    let membership = source.into_membership_with(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Err::<kube::Client, String>("no cluster reachable".into()) }
    });
    assert!(!membership.synced(), "the view is not authoritative before a listing");
    assert!(membership.snapshot().is_empty());
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(
        attempts.load(Ordering::SeqCst) >= 3,
        "the client is built again with backoff: {} attempts in 10 s",
        attempts.load(Ordering::SeqCst)
    );
    assert!(!membership.synced());
}

#[tokio::test]
async fn a_static_setup_reaches_each_peer_on_the_cluster_replication_port() {
    let setup = replication_setup_from_lookup(
        lookup(&[
            ("B2BUA_REPL", "1"),
            ("B2BUA_PEERS", "w0@10.0.0.1,w1@10.0.0.2"),
            ("B2BUA_REPL_LISTEN", "127.0.0.1:0"),
            ("B2BUA_REPL_PORT", "9100"),
        ]),
        Clock::system(),
    )
    .await
    .expect("ok")
    .expect("a static peer list needs no cluster to wire replication");
    let bound = setup.listener.local_addr();
    assert_eq!(bound.ip(), std::net::Ipv4Addr::LOCALHOST, "bound before the core spawns");
    assert_ne!(bound.port(), 0);
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
