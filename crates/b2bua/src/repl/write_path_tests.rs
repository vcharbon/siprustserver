//! The replication write path (ADR-0055 §3): a turn's write goes straight into
//! the change log and is never dropped, the serving side sends it on write, and
//! a delete stays in the log until the backup applied it.
//!
//! ```text
//!   2 000 calls flushed in one burst      every one has its change-log entry
//!   a write logged while a puller idles   it lands one transit later, no poll
//!   a delete past any tombstone TTL       kept until the peer's position covers it
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;

use super::test_support::{cref, fast_config, fwd, one_peer, supervisor_for, tick, Node};
use super::{BacklogMarks, BodySource, Changelog, ReplicatingCallStore};
use crate::metrics::B2buaMetrics;
use crate::store::{CallState, CallStore, InMemoryCallStore, PartitionRole};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9450 + n))
}

/// A wired `CallState` for `w0` over `repl`.
fn wired(repl: &Arc<ReplicatingCallStore>) -> CallState {
    let store = Arc::new(InMemoryCallStore::new()) as Arc<dyn CallStore>;
    CallState::new(store, "w0", B2buaMetrics::new()).with_replication(repl.clone())
}

/// A call of primary `w0` backed up by `w1`, built from a proxied INVITE.
fn backed_call(id: usize) -> call::Call {
    let invite = crate::test_call::invite("w0", "w1", &format!("burst-{id}"));
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    crate::initial_invite::build_initial_call(
        &invite,
        crate::test_call::src(),
        &config,
        &mut ids,
        0,
    )
}

/// **No write is dropped.** A burst of writes, flushed without any other task
/// being scheduled, each reaches the change log toward the backup.
#[tokio::test(start_paused = true)]
async fn every_write_of_a_burst_reaches_the_change_log() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    const BURST: usize = 2_000;
    for id in 0..BURST {
        let call = backed_call(id);
        state.create(call.clone());
        state.flush(&call);
    }
    tokio::task::yield_now().await;
    assert_eq!(
        repl.changelog().peer_len("w1", Partition::Bak),
        BURST,
        "every flushed call has its change-log entry toward its backup"
    );
}

/// **Sent on write.** Once a backup's puller is caught up, a write logged at
/// any instant reaches it one transit later: the serving side waits on the
/// change log, not on a poll cadence.
#[tokio::test(start_paused = true)]
async fn a_logged_write_reaches_an_idle_backup_one_transit_later() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(1), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(2), 1, &net, &clock).await;
    let b_sup =
        supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    tick(500).await;

    // Writes at offsets spread over a whole former poll period: each lands
    // within a few transits, whatever its phase against any cadence.
    for i in 0..10u64 {
        tokio::time::advance(Duration::from_millis(7 + i * 11)).await;
        let c = cref("A", &format!("w{i}"));
        a.store
            .put_call(PartitionRole::Primary, "A", &c, b"v".to_vec(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
        let mut waited = 0;
        while b.store.get_call(PartitionRole::Backup, "A", &c).await.unwrap().is_none() {
            assert!(
                waited < 5,
                "write {i} still not at the backup {waited} ms after it was logged"
            );
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            waited += 1;
        }
    }
}

/// **A delete is kept until its backup applied it.** No clock reaps a
/// tombstone the peer has not reported applying; its report does.
#[tokio::test(start_paused = true)]
async fn a_tombstone_stays_until_the_peer_reports_applying_it() {
    let clock = Clock::test_at(0);
    let log = Changelog::new(1, clock.clone());
    let _serving = log.serving("B", Partition::Bak);
    log.bump_put("B", "c", Partition::Bak);
    log.bump_delete("B", "c", Partition::Bak, false, None);
    let deleted_at = log.head();

    tokio::time::advance(Duration::from_secs(3_600)).await;
    log.reap(clock.now_ms());
    assert_eq!(log.peer_len("B", Partition::Bak), 1, "an unapplied delete outlives any TTL");

    log.note_applied("B", Partition::Bak, Watermark::new(1, deleted_at.counter - 1));
    log.reap(clock.now_ms());
    assert_eq!(log.peer_len("B", Partition::Bak), 1, "a position below the delete keeps it");

    log.note_applied("B", Partition::Bak, deleted_at);
    log.reap(clock.now_ms());
    assert_eq!(log.peer_len("B", Partition::Bak), 0, "the peer applied it: reaped");
}

fn marks(high_entries: usize, low_entries: usize) -> BacklogMarks {
    BacklogMarks {
        high_entries,
        low_entries,
        high_age: Duration::from_secs(5),
        low_age: Duration::from_secs(1),
    }
}

/// **Backlog marks.** New calls get no backup from the moment the entries
/// above the peer's reported position reach the high-water mark, or the oldest
/// of them its age, until both are under the low-water marks.
#[tokio::test(start_paused = true)]
async fn a_backlog_over_high_water_gives_new_calls_no_backup_until_under_low_water() {
    let log = Changelog::new(1, Clock::test_at(0));
    log.set_backlog_marks(marks(3, 1));
    let _serving = log.serving("B", Partition::Bak);
    assert!(log.admits_backup("B"), "an empty backlog");
    for i in 0..3 {
        log.bump_put("B", &format!("c{i}"), Partition::Bak);
    }
    assert!(!log.admits_backup("B"), "three pending entries reach the high-water mark");
    log.note_applied("B", Partition::Bak, Watermark::new(1, 2));
    assert!(!log.admits_backup("B"), "one pending entry is not under the low-water mark");
    log.note_applied("B", Partition::Bak, Watermark::new(1, 3));
    assert!(log.admits_backup("B"), "none pending: new calls get a backup again");

    log.bump_put("B", "c9", Partition::Bak);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(!log.admits_backup("B"), "the oldest pending entry reached the high-water age");
    log.note_applied("B", Partition::Bak, log.head());
    assert!(log.admits_backup("B"));
    assert!(log.admits_backup("C"), "another peer has its own backlog");
}

/// **A call admitted over high water has no backup for life.** Its writes go
/// to no peer, it is counted while it lives, and its delete propagates
/// nowhere.
#[tokio::test(start_paused = true)]
async fn a_call_admitted_over_high_water_writes_to_no_backup_for_life() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    repl.changelog().set_backlog_marks(BacklogMarks { low_entries: 0, ..marks(1, 0) });
    let state = wired(&repl);
    let first = backed_call(1);
    assert!(state.admit_backup(&first), "an empty backlog admits a backup");
    state.create(first.clone());
    state.flush(&first);
    let second = backed_call(2);
    assert!(!state.admit_backup(&second), "the first call's pending write is the high-water mark");
    state.create(second.clone());
    state.flush(&second);
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 1, "only the first call's entry");
    assert_eq!(repl.unbacked_count(), 1);
    assert!(state.hold_target(&second.call_ref).is_none(), "nothing holds for no backup");
    state.remove(&second.call_ref);
    assert_eq!(repl.unbacked_count(), 0, "the call left with its mark");
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 1, "its delete went nowhere");
}

/// **A replica refused by the backup.** The call's entry toward it goes, no
/// scan offers it again, and the call has no backup from then on.
#[tokio::test(start_paused = true)]
async fn a_backup_refusal_leaves_the_call_without_a_backup() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let call = backed_call(1);
    assert!(state.admit_backup(&call));
    state.create(call.clone());
    state.flush(&call);
    assert_eq!(repl.scan_refs_backed_by("w0", "w1"), vec![call.call_ref.clone()]);
    repl.backup_refused("w1", &call.call_ref);
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 0, "its entry went");
    assert!(repl.scan_refs_backed_by("w0", "w1").is_empty(), "no scan offers it again");
    state.flush(&call);
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 0, "a later write goes nowhere");
    assert_eq!(repl.unbacked_count(), 1);
    state.remove(&call.call_ref);
    assert_eq!(repl.unbacked_count(), 0);
}

/// **The hold reads the applied position.** A call whose backup flow is live
/// and bootstrapped holds for its current entry; the hold resolves once the
/// backup reports applying it.
#[tokio::test(start_paused = true)]
async fn a_hold_waits_for_the_backup_to_apply_the_calls_entry() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let call = backed_call(1);
    state.create(call.clone());
    state.flush(&call);
    assert!(state.hold_target(&call.call_ref).is_none(), "no flow toward the backup: no hold");
    let _serving = repl.changelog().serving("w1", Partition::Bak);
    assert!(state.hold_target(&call.call_ref).is_none(), "a flow that reported nothing: no hold");
    repl.changelog().note_applied("w1", Partition::Bak, Watermark::new(1, 0));
    let target = state.hold_target(&call.call_ref).expect("a live, bootstrapped backup");
    assert_eq!(target.at, Watermark::new(1, 1), "the call's current entry");
    let waiting = {
        let state = state.clone();
        tokio::spawn(async move { state.applied(&target).await })
    };
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "not applied yet");
    repl.changelog().note_applied("w1", Partition::Bak, Watermark::new(1, 1));
    let resolved = tokio::time::timeout(Duration::from_millis(1), waiting).await;
    assert!(resolved.is_ok(), "applied: the hold resolves");
}
