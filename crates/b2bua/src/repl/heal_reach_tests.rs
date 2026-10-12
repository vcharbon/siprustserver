//! A partition that outlasts the idle bound ends its streams (ADR-0045); its
//! heal reaches the backup within one backoff ceiling plus one connect.
//!
//! ```text
//!   t0        A pulls B's Backup flow, caught up
//!   t0        A and B partitioned: the stream is left at the idle bound and
//!             every reconnect is refused, the backoff climbing to its ceiling
//!   +30 s     B writes a call A backs up (held by the partition)
//!   +60 s     heal
//!   ≤ +64 s   A holds the call: the next backoff step reconnects from W
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use repl_net::transport::{Fault, SimulatedReplicationNetwork};
use sip_clock::Clock;
use tokio::sync::watch;

use super::test_support::{cref, fwd, tick, Node};
use super::{Puller, PullerConfig, ReplicatingCallStore};
use crate::store::{CallStore, PartitionRole};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9990 + n))
}

/// The default backoff ceiling plus the default connect bound.
const HEAL_REACH: Duration = Duration::from_secs(4);

#[tokio::test(start_paused = true)]
async fn a_backup_catches_up_within_one_backoff_ceiling_and_one_connect_after_a_long_partition() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, b) = (addr(1), addr(2));
    net.declare_endpoint("A", a);
    net.declare_endpoint("B", b);
    let node_b = Node::spawn("B", b, 3, &net, &clock).await;

    let store_a = ReplicatingCallStore::new(4, clock.clone());
    let (puller, status) = Puller::new_at(
        "B",
        "A",
        Partition::Bak,
        b,
        Arc::new(net.as_node("A")),
        store_a.clone(),
        PullerConfig::default(),
        Watermark::new(0, 0),
        crate::metrics::B2buaMetrics::new(),
    );
    let (_cancel, cancel_rx) = watch::channel(false);
    tokio::spawn(puller.run(cancel_rx));
    tick(500).await;
    assert!(status.borrow().current, "A caught up with B's Backup flow");

    net.apply_fault(Fault::Partition { a, b });
    tick(30_000).await;
    let call = cref("B", "1");
    node_b
        .store
        .put_call(PartitionRole::Primary, "B", &call, b"v1".to_vec(), &[], 0, 1, 0, &fwd("A"))
        .await
        .unwrap();
    tick(30_000).await;
    assert!(
        store_a.get_call(PartitionRole::Backup, "B", &call).await.unwrap().is_none(),
        "the partition held the write"
    );

    net.apply_fault(Fault::Heal { a, b });
    let healed = tokio::time::Instant::now();
    while store_a.get_call(PartitionRole::Backup, "B", &call).await.unwrap().is_none() {
        assert!(
            healed.elapsed() <= HEAL_REACH,
            "the backup is still behind {:?} after the heal",
            healed.elapsed()
        );
        sip_clock::testkit::advance_settled(Duration::from_millis(100)).await;
    }
}
