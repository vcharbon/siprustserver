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

use repl_net::frame::{Frame, Partition, Watermark, PROTO_VER};
use repl_net::transport::ReplicationNetwork;
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
/// The topology the router places `call` under.
fn placed(call: &call::Call) -> Option<crate::store::CallTopology> {
    crate::router::topology_cookie::placement(call)
}

fn backed_call(id: usize) -> call::Call {
    backed_call_drawn(id, 1)
}

/// The call `id` of [`backed_call`], its ids drawn at turn `turn_seq`: another
/// incarnation on the same ref.
fn backed_call_drawn(id: usize, turn_seq: u64) -> call::Call {
    let invite = crate::test_call::invite("w0", "w1", &format!("burst-{id}"));
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(turn_seq);
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
        state.create_placed(call.clone());
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

    log.note_applied("B", None, Partition::Bak, Watermark::new(1, deleted_at.counter - 1));
    log.reap(clock.now_ms());
    assert_eq!(log.peer_len("B", Partition::Bak), 1, "a position below the delete keeps it");

    log.note_applied("B", None, Partition::Bak, deleted_at);
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
    log.note_applied("B", None, Partition::Bak, Watermark::new(1, 2));
    assert!(!log.admits_backup("B"), "one pending entry is not under the low-water mark");
    log.note_applied("B", None, Partition::Bak, Watermark::new(1, 3));
    assert!(log.admits_backup("B"), "none pending: new calls get a backup again");

    log.bump_put("B", "c9", Partition::Bak);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(!log.admits_backup("B"), "the oldest pending entry reached the high-water age");
    log.note_applied("B", None, Partition::Bak, log.head());
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
    assert!(
        state.admit_backup(&first.call_ref, placed(&first).as_ref()),
        "an empty backlog admits a backup"
    );
    state.create_placed(first.clone());
    state.flush(&first);
    let second = backed_call(2);
    assert!(
        !state.admit_backup(&second.call_ref, placed(&second).as_ref()),
        "the first call's pending write is the high-water mark"
    );
    state.create_placed(second.clone());
    state.flush(&second);
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 1, "only the first call's entry");
    assert_eq!(repl.unbacked_count(), 1);
    assert!(state.hold_target(&second.call_ref).is_none(), "nothing holds for no backup");
    state.remove(&second.call_ref);
    assert_eq!(repl.unbacked_count(), 0, "the call left with its mark");
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 1, "its delete went nowhere");
}

/// **A new call on a ref is not unbacked for an earlier call's mark.** The
/// earlier call on the identity had no backup and its delete ended it alone;
/// the retry admitted with its backup has one.
#[tokio::test(start_paused = true)]
async fn a_backed_retry_on_the_ref_clears_the_earlier_calls_mark() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let retry = backed_call(1);
    repl.leave_unbacked(&retry.call_ref);
    assert!(state.admit_backup(&retry.call_ref, placed(&retry).as_ref()));
    assert!(!repl.unbacked(&retry.call_ref), "the retry has its backup");
}

/// **A replica refused by the backup.** The call's entry toward it goes, no
/// scan offers it again, and the call has no backup from then on.
#[tokio::test(start_paused = true)]
async fn a_backup_refusal_leaves_the_call_without_a_backup() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let call = backed_call(1);
    assert!(state.admit_backup(&call.call_ref, placed(&call).as_ref()));
    state.create_placed(call.clone());
    state.flush(&call);
    assert_eq!(repl.scan_refs_backed_by("w0", "w1"), vec![call.call_ref.clone()]);
    repl.backup_refused("w1", &call.call_ref, Some(call.incarnation()));
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 0, "its entry went");
    assert!(repl.scan_refs_backed_by("w0", "w1").is_empty(), "no scan offers it again");
    state.flush(&call);
    assert_eq!(repl.changelog().peer_len("w1", Partition::Bak), 0, "a later write goes nowhere");
    assert_eq!(repl.unbacked_count(), 1);
    state.remove(&call.call_ref);
    assert_eq!(repl.unbacked_count(), 0);
}

/// **A refusal names the call it refuses.** The backup's refusal of an earlier
/// call on the ref, in flight while the retry is admitted with its backup,
/// leaves the retry backed; the retry's own refusal unbacks it.
#[tokio::test(start_paused = true)]
async fn a_refusal_of_an_earlier_call_on_the_ref_leaves_the_retry_backed() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let first = backed_call_drawn(1, 1);
    assert!(state.admit_backup(&first.call_ref, placed(&first).as_ref()));
    state.create_placed(first.clone());
    state.flush(&first);
    state.remove(&first.call_ref);
    let retry = backed_call_drawn(1, 2);
    assert_eq!(retry.call_ref, first.call_ref);
    assert_ne!(retry.incarnation(), first.incarnation());
    assert!(state.admit_backup(&retry.call_ref, placed(&retry).as_ref()));
    state.create_placed(retry.clone());
    state.flush(&retry);
    repl.backup_refused("w1", &retry.call_ref, Some(first.incarnation()));
    assert!(!repl.unbacked(&retry.call_ref), "the earlier call's refusal leaves the retry backed");
    repl.backup_refused("w1", &retry.call_ref, Some(retry.incarnation()));
    assert!(repl.unbacked(&retry.call_ref), "its own refusal unbacks it");
    state.remove(&retry.call_ref);
}

/// **The hold reads the applied position.** A call whose backup flow is live
/// and bootstrapped holds for its current entry; the hold resolves once the
/// backup reports applying it.
#[tokio::test(start_paused = true)]
async fn a_hold_waits_for_the_backup_to_apply_the_calls_entry() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let call = backed_call(1);
    state.create_placed(call.clone());
    state.flush(&call);
    assert!(state.hold_target(&call.call_ref).is_none(), "no flow toward the backup: no hold");
    let _serving = repl.changelog().serving("w1", Partition::Bak);
    assert!(state.hold_target(&call.call_ref).is_none(), "a flow that reported nothing: no hold");
    repl.changelog().note_applied("w1", None, Partition::Bak, Watermark::new(1, 0));
    let target = state.hold_target(&call.call_ref).expect("a live, bootstrapped backup");
    assert_eq!(target.at, Watermark::new(1, 1), "the call's current entry");
    let waiting = held(&state, &call.call_ref);
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "not applied yet");
    repl.changelog().note_applied("w1", None, Partition::Bak, Watermark::new(1, 1));
    let resolved = tokio::time::timeout(Duration::from_millis(1), waiting).await;
    assert!(resolved.is_ok(), "applied: the hold resolves");
}

/// A task waiting for `call_ref`'s backup to apply its current entry.
fn held(state: &CallState, call_ref: &str) -> tokio::task::JoinHandle<()> {
    let (state, call_ref) = (state.clone(), call_ref.to_string());
    tokio::spawn(async move { state.applied_current(&call_ref).await })
}

/// **Another call's applied write never releases a hold.** A is held at its
/// entry 1; B writes at 2; A writes again and moves to 3 before the serve task
/// drains. The backup reports 2 (B applied, A not yet): A's hold stays; it
/// resolves at 3.
#[tokio::test(start_paused = true)]
async fn a_hold_follows_its_calls_entry_past_another_calls_applied_write() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let _serving = repl.changelog().serving("w1", Partition::Bak);
    repl.changelog().note_applied("w1", None, Partition::Bak, Watermark::new(1, 0));
    let (a, b) = (backed_call(1), backed_call(2));
    state.create_placed(a.clone());
    state.create_placed(b.clone());
    state.flush(&a);
    let waiting = held(&state, &a.call_ref);
    tokio::task::yield_now().await;
    state.flush(&b);
    state.flush(&a);
    assert_eq!(
        repl.changelog().position_of("w1", Partition::Bak, &a.call_ref),
        Some(Watermark::new(1, 3))
    );
    repl.changelog().note_applied("w1", None, Partition::Bak, Watermark::new(1, 2));
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "B's applied write does not stand for A's");
    repl.changelog().note_applied("w1", None, Partition::Bak, Watermark::new(1, 3));
    let resolved = tokio::time::timeout(Duration::from_millis(1), waiting).await;
    assert!(resolved.is_ok(), "A's current entry applied: the hold resolves");
}

/// **A refusal frame reaches the primary's store.** The backup's
/// `ReplicaRefused` on its Backup flow leaves the primary's call without a
/// backup: its entry toward the backup goes and no scan offers it again.
#[tokio::test(start_paused = true)]
async fn a_backups_refusal_frame_leaves_the_primarys_call_without_a_backup() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(5), 1, &net, &clock).await;
    let c = cref("A", "refused");
    let fwd_b = fwd("B");
    let body = b"v".to_vec();
    a.store.put_call(PartitionRole::Primary, "A", &c, body, &[], 0, 1, 0, &fwd_b).await.unwrap();
    let conn = net.connect(a.addr).await.unwrap();
    let pull = Frame::PullRequest {
        proto_ver: PROTO_VER,
        caller: "B".into(),
        identity: None,
        partition: Partition::Bak,
        since: Watermark::new(1, 0),
    };
    conn.send(pull).await.unwrap();
    let (at, incarnation) = loop {
        match conn.recv().await {
            Some(Frame::Data { at, call_ref, incarnation, .. }) if call_ref == c => {
                break (at, incarnation)
            }
            Some(_) => continue,
            None => panic!("the stream closed before the call's write"),
        }
    };
    conn.send(Frame::ReplicaRefused { at, call_ref: c.clone(), incarnation }).await.unwrap();
    tick(50).await;
    assert!(a.store.unbacked(&c), "the primary's call has no backup");
    assert_eq!(a.store.changelog().peer_len("B", Partition::Bak), 0, "its entry went");
    assert!(a.store.scan_refs_backed_by("A", "B").is_empty(), "no scan offers it again");
}

/// **A body that does not encode gives up the call's backup.** The call's
/// earlier write reached the change log; a later one nests past the bound
/// every peer decodes under: the call has no backup from then on, and its entry
/// toward the backup becomes a delete of the earlier replica.
#[tokio::test(start_paused = true)]
async fn a_body_that_does_not_encode_leaves_the_call_without_a_backup() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = wired(&repl);
    let call = backed_call(1);
    state.create_placed(call.clone());
    state.flush(&call);
    let deep = (0..200).fold(serde_json::Value::Null, |v, _| serde_json::json!([v]));
    let deep = call::helpers::set_call_ext(call.clone(), "svc", Some(deep));
    state.update(deep.clone());
    state.flush(&deep);
    assert!(repl.unbacked(&call.call_ref), "the call has no backup");
    let frames = repl
        .changelog()
        .drain_since(
            "w1",
            Partition::Bak,
            Watermark::new(1, 0),
            10,
            &*repl,
            PartitionRole::Primary,
            "w0",
        )
        .await;
    assert!(
        matches!(frames.as_slice(), [Frame::Data { op: repl_net::frame::Op::Delete, .. }]),
        "the entry toward the backup is a delete: {frames:?}"
    );
    state.remove(&call.call_ref);
    assert_eq!(repl.unbacked_count(), 0);
}

/// **A puller's position counts under the identity membership binds.** B's
/// supervisor states B's worker identity on the flow it opens toward A; A
/// counts B's applied position only once its membership binds ordinal B to
/// that identity (ADR-0055 §6).
#[tokio::test(start_paused = true)]
async fn a_pullers_position_counts_under_the_identity_membership_binds() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(6), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(7), 1, &net, &clock).await;
    a.store.changelog().bind_identity("B", Some("uid-other"));
    let a_addr = a.addr;
    let resolve = Arc::new(super::FnPeerResolver(move |_: &topology::Peer| a_addr));
    let b_sup = super::ReplicationSupervisor::new(
        "B",
        Some("uid-b".into()),
        net.clone(),
        b.store.clone(),
        resolve,
        fast_config(),
        B2buaMetrics::new(),
    );
    b_sup.start(one_peer("A", &clock));
    tick(6_000).await;
    assert!(!a.store.changelog().flow_caught_up("B", Partition::Pri), "another identity's flow");
    a.store.changelog().bind_identity("B", Some("uid-b"));
    tick(6_000).await;
    assert!(a.store.changelog().flow_caught_up("B", Partition::Pri), "B's identity counts");
}
