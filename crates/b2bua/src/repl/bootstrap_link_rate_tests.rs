//! A bootstrap of large bodies over a slow link completes: every batch the
//! scan sends leaves within the server's send stall, so the stream is never
//! dropped as a peer that stopped reading, and the backup holds every call
//! (ADR-0014 §8, ADR-0045).
//!
//! ```text
//!   link         4 MiB/s each way, a 1 MiB sender's buffer, 1 ms transit
//!   bodies       12 calls of 6 MiB: 1.5 s each on the link, 18 s in all
//!   server       send_stall 15 s (the default)
//!   puller       idle bound 2 s: a frame lands every 1.5 s, never idle
//!   one batch    of the 12 bodies blocks past 15 s: dropped at 15 s with 11
//!                queued, re-scanned cold in the same order, dropped again
//!   batches      of one body each leave in 1.5 s: done by ~18 s
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::{LinkRate, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, supervisor_for, tick};
use super::{Changelog, ReplServer, ReplicatingCallStore};
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;
const MIB: usize = 1024 * 1024;

/// The calls A holds when B bootstraps.
const CALLS: usize = 12;
/// Each call's body: 1.5 s on the link.
const BODY: usize = 6 * MIB;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9960 + n))
}

/// A fabric carrying 4 MiB/s each way behind a 1 MiB sender's buffer.
fn slow_link() -> Arc<SimulatedReplicationNetwork> {
    let rate = LinkRate { bytes_per_sec: 4 * MIB as u64, buffer_bytes: MIB as u64 };
    Arc::new(SimulatedReplicationNetwork::with_link_rate(1, rate))
}

/// Node A serving its changelog on `net` under the default link bounds.
async fn primary(net: &Arc<SimulatedReplicationNetwork>, clock: &Clock) -> ReplicatingCallStore {
    let changelog = Changelog::new(1, clock.clone());
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let listener = net.listen(addr(1)).await.unwrap();
    tokio::spawn(ReplServer::new("A", changelog, Arc::new(store.clone())).run(listener));
    store
}

/// B, pulling A.
fn backup(net: &Arc<SimulatedReplicationNetwork>, clock: &Clock) -> ReplicatingCallStore {
    let store = ReplicatingCallStore::new(2, clock.clone());
    let sup = supervisor_for("B", &store, net, clock, vec![("A".into(), addr(1))], fast_config());
    sup.start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone())));
    store
}

/// The ids of A's calls B does not hold.
async fn missing(b: &ReplicatingCallStore) -> Vec<String> {
    let mut missing = Vec::new();
    for n in 0..CALLS {
        let id = format!("c{n}");
        if b.get_call(BAK, "A", &cref("A", &id)).await.unwrap().is_none() {
            missing.push(id);
        }
    }
    missing
}

#[tokio::test(start_paused = true)]
async fn a_bootstrap_of_large_bodies_on_a_slow_link_backs_up_every_call() {
    let clock = Clock::test_at(0);
    let net = slow_link();
    let a = primary(&net, &clock).await;
    for n in 0..CALLS {
        let call_ref = cref("A", &format!("c{n}"));
        a.put_call(PRI, "A", &call_ref, vec![n as u8; BODY], &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }

    let b = backup(&net, &clock);
    tick(40_000).await;

    assert_eq!(missing(&b).await, Vec::<String>::new(), "the bootstrap carries every call");
}
