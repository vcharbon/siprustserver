//! A cold scan the server drops at the same body on every pass, past the
//! puller's bootstrap hard timer (ADR-0011 X5): every pass counts as dropped
//! on its flow, before and after the timer, the timer runs out once on that
//! flow alone, and the calls past the drop point are not held. A stream left
//! after its first catch-up `Noop` counts as neither; an opening `PullRequest`
//! that cannot be sent counts as dropped.
//!
//! ```text
//!   link     4 MiB/s each way, a 1 MiB sender's buffer, 1 ms transit
//!   server   send_stall 1 s
//!   calls    10 of 1 KiB and two of 8 MiB: the first large one the scan
//!            meets blocks its send ~1.75 s > 1 s, so every scan of the
//!            Backup flow is dropped there and the second never leaves,
//!            whatever order the scan takes; the puller reads the first
//!            drop ~2 s after the Backup flow opens
//!   puller   fast_test, hard timer 3 s: past the first drop; backoff
//!            100 ms to 1 s
//!   Reclaim  B's own calls at A: none, so that scan reaches its Noop
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::{Fault, LinkRate, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, supervisor_for, tick, Node};
use super::{
    Changelog, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor, ServeConfig,
};
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;
const MIB: usize = 1024 * 1024;

/// The calls A holds when B bootstraps.
const CALLS: usize = 12;
/// The calls whose bodies no batch can send within the send stall.
const LARGE: [usize; 2] = [3, 8];

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9980 + n))
}

fn body(n: usize) -> Vec<u8> {
    let len = if LARGE.contains(&n) { 8 * MIB } else { 1024 };
    vec![n as u8; len]
}

/// A fabric carrying 4 MiB/s each way behind a 1 MiB sender's buffer.
fn slow_link() -> Arc<SimulatedReplicationNetwork> {
    let rate = LinkRate { bytes_per_sec: 4 * MIB as u64, buffer_bytes: MIB as u64 };
    Arc::new(SimulatedReplicationNetwork::with_link_rate(1, rate))
}

/// Node A serving its changelog on `net`, a send blocked 1 s leaving the
/// stream, holding [`CALLS`] calls backed up on B.
async fn primary(net: &Arc<SimulatedReplicationNetwork>, clock: &Clock) {
    let changelog = Changelog::new(1, clock.clone());
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let listener = net.listen(addr(1)).await.unwrap();
    let serve = ServeConfig { send_stall: Duration::from_secs(1), ..ServeConfig::default() };
    let server = ReplServer::new("A", changelog, Arc::new(store.clone())).with_config(serve);
    tokio::spawn(server.run(listener));
    for n in 0..CALLS {
        let call_ref = cref("A", &format!("c{n}"));
        store.put_call(PRI, "A", &call_ref, body(n), &[], 0, 1, 0, &fwd("B")).await.unwrap();
    }
}

/// B, pulling A.
fn backup(
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
) -> (ReplicatingCallStore, ReplicationSupervisor) {
    let store = ReplicatingCallStore::new(2, clock.clone());
    let config = PullerConfig { bootstrap_hard_timeout_ms: 3_000, ..fast_config() };
    let sup = supervisor_for("B", &store, net, clock, vec![("A".into(), addr(1))], config);
    sup.start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone())));
    (store, sup)
}

#[tokio::test(start_paused = true)]
async fn a_scan_dropped_at_the_same_body_counts_every_pass_and_one_timer() {
    let clock = Clock::test_at(0);
    let net = slow_link();
    primary(&net, &clock).await;
    let (b, sup) = backup(&net, &clock);
    let metrics = sup.metrics();

    tick(2_500).await;
    assert!(
        metrics.repl_bootstrap_dropped("backup") >= 1,
        "the first scan was dropped before the timer ran out"
    );
    assert_eq!(metrics.repl_bootstrap_stalled_total(), 0, "the 3 s timer has not run out");

    tick(1_000).await;
    let dropped_past_timer = metrics.repl_bootstrap_dropped("backup");
    assert_eq!(metrics.repl_bootstrap_stalled_total(), 1, "the Backup flow's timer ran out");

    tick(27_000).await;
    assert!(
        metrics.repl_bootstrap_dropped("backup") >= dropped_past_timer + 10,
        "a pass every ~2 s is dropped again: {} then {}",
        dropped_past_timer,
        metrics.repl_bootstrap_dropped("backup")
    );
    assert_eq!(metrics.repl_bootstrap_dropped("recovery"), 0, "the Reclaim scan reached its Noop");
    assert_eq!(metrics.repl_bootstrap_stalled_total(), 1, "the timer runs out once per bootstrap");
    assert!(held(&b).await < CALLS, "the calls past the drop point are not held");
    assert!(!sup.all_current(), "the Backup flow never caught up");
}

/// How many of A's calls B holds.
async fn held(b: &ReplicatingCallStore) -> usize {
    let mut held = 0;
    for n in 0..CALLS {
        if b.get_call(BAK, "A", &cref("A", &format!("c{n}"))).await.unwrap().is_some() {
            held += 1;
        }
    }
    held
}

/// Both flows reach their first `Noop`, then a partition outlasts the 2 s
/// idle bound, so each stream is left and each flow pulls again warm.
#[tokio::test(start_paused = true)]
async fn a_stream_left_after_its_noop_is_not_a_dropped_bootstrap() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;
    net.declare_endpoint("A", a.addr);
    net.declare_endpoint("B", b.addr);
    let early = cref("A", "early");
    a.store.put_call(PRI, "A", &early, b"e".to_vec(), &[], 0, 1, 0, &fwd("B")).await.unwrap();
    let sup =
        supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    sup.start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone())));
    tick(500).await;
    assert!(sup.all_current(), "both scans reached their Noop");

    net.apply_fault(Fault::Partition { a: b.addr, b: a.addr });
    tick(3_000).await;
    net.apply_fault(Fault::Heal { a: b.addr, b: a.addr });
    let late = cref("A", "late");
    a.store.put_call(PRI, "A", &late, b"l".to_vec(), &[], 0, 1, 0, &fwd("B")).await.unwrap();
    tick(3_000).await;

    assert_eq!(b.store.get_call(BAK, "A", &late).await.unwrap().as_deref(), Some(&b"l"[..]));
    let metrics = sup.metrics();
    assert_eq!(metrics.repl_bootstrap_dropped("recovery"), 0);
    assert_eq!(metrics.repl_bootstrap_dropped("backup"), 0);
    assert_eq!(metrics.repl_bootstrap_stalled_total(), 0, "no timer ran out");
}

/// B's direction to A is cut before B starts: every connect succeeds and its
/// opening `PullRequest` fails, so each pull of the Reclaim flow counts as
/// dropped with no scan. The hard timer is raised past the scenario, so the
/// Backup flow opens only once the heal lets the Reclaim scan reach its `Noop`.
#[tokio::test(start_paused = true)]
async fn a_pull_request_that_cannot_be_sent_counts_as_a_dropped_bootstrap() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(21), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(22), 1, &net, &clock).await;
    net.declare_endpoint("A", a.addr);
    net.declare_endpoint("B", b.addr);
    let call = cref("A", "c");
    a.store.put_call(PRI, "A", &call, b"c".to_vec(), &[], 0, 1, 0, &fwd("B")).await.unwrap();
    net.apply_fault(Fault::Cut { src: b.addr, dst: a.addr });
    let config = PullerConfig { bootstrap_hard_timeout_ms: 10_000, ..fast_config() };
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], config);
    sup.start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone())));
    let metrics = sup.metrics();

    tick(1_500).await;
    let dropped_while_cut = metrics.repl_bootstrap_dropped("recovery");
    assert!(dropped_while_cut >= 2, "each pull under the cut is dropped: {dropped_while_cut}");
    assert!(!sup.all_current());

    net.apply_fault(Fault::Heal { a: b.addr, b: a.addr });
    tick(3_000).await;
    assert!(sup.all_current(), "both scans reached their Noop once healed");
    assert_eq!(b.store.get_call(BAK, "A", &call).await.unwrap().as_deref(), Some(&b"c"[..]));
    assert_eq!(metrics.repl_bootstrap_dropped("recovery"), dropped_while_cut);
    assert_eq!(metrics.repl_bootstrap_dropped("backup"), 0);
    assert_eq!(metrics.repl_bootstrap_stalled_total(), 0, "no timer ran out");
}
