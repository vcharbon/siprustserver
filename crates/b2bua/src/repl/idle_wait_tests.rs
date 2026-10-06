//! The supervisor's waits on puller status go idle between status changes.
//!
//! The backup-deferral gate and [`ReplicationSupervisor::await_current`] wait
//! on the Reclaim pullers' status watches while a peer's flow is not ready; a
//! puller waits on its cancel watch while it connects, backs off and streams.
//! A wait that returns at once on a status it already read, or on a channel
//! whose sender is gone, never yields to the timer: under a paused clock the
//! runtime never goes idle and time stops. Each scenario runs under
//! [`run_paused_within`], so a spinning wait fails the test in bounded real
//! time instead of hanging it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use repl_net::transport::{ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::testkit::run_paused_within;
use sip_clock::Clock;
use tokio::sync::watch;
use topology::Peer;

use super::test_support::{fast_config, one_peer, supervisor_for};
use super::{FnPeerResolver, Puller, ReplicatingCallStore, ReplicationSupervisor};

/// Real time a scenario gets before it counts as spinning: each one finishes
/// in milliseconds once its waits go idle.
const WALL: Duration = Duration::from_secs(10);

/// Virtual time each scenario sleeps across: past the 2 s bootstrap hard timer
/// of [`fast_config`], so every status the pullers publish while the peer is
/// not ready has been published.
const ACROSS: Duration = Duration::from_secs(3);

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9900 + n))
}

/// A peer that accepts every replication connection, reads its pull request
/// and never answers: the puller reaches it (`ever_connected`) and its flow
/// stays not current, so the backup gate keeps waiting.
async fn silent_peer(net: &Arc<SimulatedReplicationNetwork>, at: SocketAddr) {
    let listener = net.listen(at).await.unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(conn) = listener.accept().await {
            let _ = conn.recv().await;
            held.push(conn);
        }
    });
}

/// Node A pulling a [`silent_peer`] B.
async fn pulling_a_silent_peer(n: u16) -> ReplicationSupervisor {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(n);
    silent_peer(&net, b).await;
    let store = ReplicatingCallStore::new(2, clock.clone());
    let sup = supervisor_for("A", &store, &net, &clock, vec![("B".into(), b)], fast_config());
    sup.start(one_peer("B", &clock));
    sup
}

#[test]
fn the_backup_gate_goes_idle_while_a_reached_peer_is_not_current() {
    let outcome = run_paused_within(WALL, || async {
        let sup = pulling_a_silent_peer(1).await;
        tokio::time::sleep(ACROSS).await;
        (sup.all_bootstrapped(), sup.all_current())
    });
    assert_eq!(
        outcome,
        Some((true, false)),
        "the gate waits for the next status change of a reached, not-current peer \
         (None: the paused clock never went idle)"
    );
}

#[test]
fn the_backup_gate_goes_idle_once_a_reclaim_puller_has_ended() {
    let outcome = run_paused_within(WALL, || async {
        let clock = Clock::test_at(0);
        let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let store = ReplicatingCallStore::new(2, clock.clone());
        // The puller's first resolve panics: its task ends and drops its status
        // sender while the supervisor still holds the receiver.
        let resolve = Arc::new(FnPeerResolver(|_: &Peer| -> SocketAddr {
            panic!("expected: the peer resolver fails")
        }));
        let sup = ReplicationSupervisor::with_config("A", net, store, resolve, fast_config());
        sup.start(one_peer("B", &clock));
        tokio::time::sleep(ACROSS).await;
        sup.all_current()
    });
    assert_eq!(
        outcome,
        Some(false),
        "the gate falls back to its poll once no puller can publish \
         (None: the paused clock never went idle)"
    );
}

#[test]
fn awaiting_a_reached_peer_current_goes_idle_while_it_is_not() {
    let outcome = run_paused_within(WALL, || async {
        let sup = pulling_a_silent_peer(2).await;
        tokio::select! {
            () = sup.await_current("B") => "current",
            () = tokio::time::sleep(ACROSS) => "still waiting",
        }
    });
    assert_eq!(
        outcome,
        Some("still waiting"),
        "await_current waits for the next status change \
         (None: the paused clock never went idle)"
    );
}

/// A puller toward `peer` on `net`, running until `cancel_rx` cancels it.
fn spawn_puller(
    net: Arc<SimulatedReplicationNetwork>,
    peer: SocketAddr,
    cancel_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let (puller, _status) = Puller::new_at(
        "B",
        "A",
        Partition::Pri,
        peer,
        net as Arc<dyn ReplicationNetwork>,
        ReplicatingCallStore::new(2, Clock::test_at(0)),
        fast_config(),
        Watermark::new(0, 0),
        crate::metrics::B2buaMetrics::new(),
    );
    tokio::spawn(puller.run(cancel_rx))
}

#[test]
fn a_streaming_puller_whose_cancel_sender_is_dropped_stops() {
    let outcome = run_paused_within(WALL, || async {
        let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let b = addr(3);
        silent_peer(&net, b).await;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let run = spawn_puller(net, b, cancel_rx);
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(cancel_tx);
        tokio::time::sleep(ACROSS).await;
        run.is_finished()
    });
    assert_eq!(
        outcome,
        Some(true),
        "nothing can park a puller whose cancel sender is gone, so it stops \
         (None: the paused clock never went idle)"
    );
}

#[test]
fn a_backing_off_puller_whose_cancel_sender_is_dropped_stops() {
    let outcome = run_paused_within(WALL, || async {
        // Nothing listens at the peer's address: every connect fails and the
        // puller sits in its 100 ms backoff when the sender goes.
        let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let run = spawn_puller(net, addr(4), cancel_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!run.is_finished(), "the puller backs off toward an unreachable peer");
        drop(cancel_tx);
        tokio::time::sleep(ACROSS).await;
        run.is_finished()
    });
    assert_eq!(
        outcome,
        Some(true),
        "a backoff ends in cancellation once the cancel sender is gone \
         (None: the paused clock never went idle)"
    );
}
