//! One replication protocol version per stream: a server refuses a
//! `PullRequest` announcing another version with one `StreamRefused` and
//! nothing else, and a puller refused so leaves the stream, counts it and
//! pulls again after its backoff.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Partition, Watermark, PROTO_VER};
use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::watch;

use super::test_support::{cref, fwd, tick, Node};
use super::{Puller, PullerConfig, ReplicatingCallStore};
use crate::metrics::B2buaMetrics;
use crate::store::{CallStore, PartitionRole};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9480 + n))
}

#[tokio::test(start_paused = true)]
async fn a_pull_announcing_another_version_is_refused_with_nothing_else() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(1), 1, &net, &clock).await;
    let c = cref("A", "1");
    a.store
        .put_call(PartitionRole::Primary, "A", &c, b"v".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();

    let conn = net.connect(a.addr).await.unwrap();
    let older = PROTO_VER - 1;
    let pull = Frame::PullRequest {
        proto_ver: older,
        caller: "B".into(),
        identity: None,
        partition: Partition::Bak,
        since: Watermark::new(0, 0),
    };
    conn.send(pull).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), conn.recv()).await;
    match first {
        Ok(Some(Frame::StreamRefused { proto_ver, reason })) => {
            assert_eq!(proto_ver, PROTO_VER, "the refusal states the server's version");
            assert!(reason.contains(&PROTO_VER.to_string()), "{reason}");
        }
        other => panic!("the server served a peer of another version: {other:?}"),
    }
    let next = tokio::time::timeout(Duration::from_secs(10), conn.recv()).await;
    assert!(matches!(next, Ok(None)), "nothing after the refusal, the stream closed: {next:?}");
}

#[tokio::test(start_paused = true)]
async fn a_refused_puller_counts_it_and_pulls_again_after_its_backoff() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let listener = net.listen(addr(2)).await.unwrap();
    let pulls = Arc::new(std::sync::Mutex::new(0usize));
    let seen = pulls.clone();
    tokio::spawn(async move {
        while let Ok(conn) = listener.accept().await {
            if let Some(Frame::PullRequest { proto_ver, .. }) = conn.recv().await {
                assert_eq!(proto_ver, PROTO_VER, "the puller announces this node's version");
                *seen.lock().unwrap() += 1;
                let refused =
                    Frame::StreamRefused { proto_ver: PROTO_VER + 1, reason: "newer".into() };
                let _ = conn.send(refused).await;
            }
        }
    });
    let metrics = B2buaMetrics::new();
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let (puller, status) = Puller::new_at(
        "A",
        "B",
        Partition::Bak,
        addr(2),
        net.clone() as Arc<dyn ReplicationNetwork>,
        store,
        PullerConfig::fast_test(),
        Watermark::new(0, 0),
        metrics.clone(),
    );
    let (_cancel, cancel_rx) = watch::channel(false);
    tokio::spawn(puller.run(cancel_rx));
    tick(1_000).await;
    assert!(*pulls.lock().unwrap() >= 2, "the puller pulled again after its backoff");
    assert!(metrics.repl_streams_refused_total() >= 2, "each refusal is counted");
    assert!(!status.borrow().current, "a refused stream never reads as current");
}

/// v6: a retired dialog is a bare state, its open records in the dialog's
/// requests (ADR-0055 §2), so a v5 peer's records do not decode here.
#[test]
fn the_version_names_the_retired_dialog_shape() {
    assert_eq!(PROTO_VER, 6);
}
