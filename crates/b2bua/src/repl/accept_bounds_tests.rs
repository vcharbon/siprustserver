//! The serve loop's accept side is bounded (ADR-0045): a connection that
//! sends no opening `PullRequest` within the open timeout is closed, and a
//! connection above the cap on concurrent connections is closed at once. A
//! peer that opens its flow at once is served past the open timeout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Partition, Watermark, PROTO_VER};
use repl_net::transport::{ReplicationConnection, ReplicationNetwork};
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;

use super::{Changelog, ReplServer, ReplicatingCallStore, ServeConfig};
use crate::metrics::B2buaMetrics;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9970 + n))
}

/// A server for node A on `net` at `at`, under the default serve config.
async fn serve(net: &Arc<SimulatedReplicationNetwork>, at: SocketAddr) -> B2buaMetrics {
    let clock = Clock::test_at(0);
    let changelog = Changelog::new(1, clock.clone());
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock);
    let metrics = B2buaMetrics::new();
    let server = ReplServer::new("A", changelog, Arc::new(store)).with_metrics(metrics.clone());
    tokio::spawn(server.run(net.listen(at).await.unwrap()));
    metrics
}

/// Whether the server closed `conn` within a millisecond: `false` while it
/// stays open with nothing to read.
async fn closed(conn: &dyn ReplicationConnection) -> bool {
    matches!(tokio::time::timeout(Duration::from_millis(1), conn.recv()).await, Ok(None))
}

fn pull(caller: &str) -> Frame {
    Frame::PullRequest {
        proto_ver: PROTO_VER,
        caller: caller.into(),
        identity: None,
        partition: Partition::Bak,
        since: Watermark::new(0, 0),
    }
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_sends_no_frame_is_closed_at_the_open_timeout() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let metrics = serve(&net, addr(1)).await;
    let open = ServeConfig::default().open_timeout;

    let silent = net.connect(addr(1)).await.unwrap();
    let pulling = net.connect(addr(1)).await.unwrap();
    pulling.send(pull("B")).await.unwrap();
    tokio::time::sleep(open - Duration::from_millis(10)).await;
    assert!(!closed(silent.as_ref()).await, "open until the open timeout");
    assert_eq!(metrics.repl_inbound_dropped("open_timeout"), 0);

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(closed(silent.as_ref()).await, "closed at the open timeout");
    assert_eq!(metrics.repl_inbound_dropped("open_timeout"), 1);

    let mut served = false;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(1), pulling.recv()).await
    {
        served |= matches!(frame, Frame::Noop { .. });
    }
    assert!(served, "the peer that opened its flow is served past the open timeout");
    assert!(!closed(pulling.as_ref()).await, "and its stream stays open");
}

#[tokio::test(start_paused = true)]
async fn a_connection_above_the_cap_is_closed_at_once_and_counted() {
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let metrics = serve(&net, addr(2)).await;
    let config = ServeConfig::default();

    let mut held = Vec::new();
    for _ in 0..config.max_connections {
        held.push(net.connect(addr(2)).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    let above = net.connect(addr(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(closed(above.as_ref()).await, "the connection above the cap is closed at once");
    assert_eq!(metrics.repl_inbound_dropped("cap"), 1);
    for conn in &held {
        assert!(!closed(conn.as_ref()).await, "the connections under the cap stay open");
    }

    // The silent ones leave at the open timeout; their slots serve a peer again.
    tokio::time::sleep(config.open_timeout).await;
    let again = net.connect(addr(2)).await.unwrap();
    again.send(pull("B")).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), again.recv()).await;
    assert!(matches!(first, Ok(Some(_))), "a freed slot serves a peer: {first:?}");
    assert_eq!(metrics.repl_inbound_dropped("cap"), 1);
    assert_eq!(metrics.repl_inbound_dropped("open_timeout"), config.max_connections as u64);
}
