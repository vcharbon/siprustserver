//! The replica reap shares its task with the call reaper's sweep, whose
//! aborts free a hung turn: a ghost whose lock a turn holds is ended once
//! that turn let the lock go, whatever the turn did to the call's queue, and
//! the reap never waits on that lock.

use std::time::Duration;

use b2bua_sdk::event::CallEvent;

use crate::metrics::RemovalClass;
use crate::router::test_support::{answered, node_with, Node};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// A node whose sweep runs every second, backing up `w0`, which departed
/// leaving a call no node ended whose replica outlives its TTL a second on:
/// the ghost's callRef.
async fn ghost_of_a_departed_primary(n: &Node) -> String {
    n.membership.add(topology::Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ghost = answered("w0", "w1", "ghost");
    let r = ghost.call_ref.clone();
    let body = crate::store::envelope::placed_body(&ghost).unwrap();
    let keys = call::call_index_keys(&ghost);
    let opts = PutOpts::default();
    let bak = PartitionRole::Backup;
    n.store.put_call(bak, "w0", &r, body, &keys, 1_000, 1, 0, &opts).await.unwrap();
    n.membership.remove("w0");
    r
}

/// The ghost's one CDR is written and nothing of it is left.
async fn ended_once(n: &Node, r: &str) {
    let ctx = n.core.router_ctx();
    sip_clock::testkit::settle().await;
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|c| c.call_ref).collect();
    assert_eq!(cdrs, [r.to_string()], "the ghost's one CDR, written once its lock is free");
    assert!(!ctx.state.contains(r), "no copy is left");
    assert!(!ctx.dispatcher.has_queue(r), "no queue is left behind");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

#[tokio::test(start_paused = true)]
async fn a_ghost_reap_meeting_a_held_lock_never_stalls_the_reaper() {
    let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 1).await;
    let ctx = n.core.router_ctx();
    let r = ghost_of_a_departed_primary(&n).await;
    let held = ctx.state.lock(&r).await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    let sweeps = n.metrics.reaper_sweeps_total();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(n.metrics.reaper_sweeps_total() >= sweeps + 3, "the reaper sweeps on");
    assert!(n.cdr.snapshot().is_empty(), "the ghost waits for the lock");

    drop(held);
    ended_once(&n, &r).await;
}

/// The turn holding the ghost's lock queued its call's release, as a
/// shedding copy's turn does: the ghost is still ended after that turn.
#[tokio::test(start_paused = true)]
async fn a_ghost_whose_lock_holder_queued_its_release_is_still_ended() {
    let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 1).await;
    let ctx = n.core.router_ctx();
    let r = ghost_of_a_departed_primary(&n).await;
    let held = ctx.state.lock(&r).await;
    // A turn of the call waits on the lock; its queue's release is queued.
    let event = CallEvent::InternalEvent {
        call_ref: r.clone(),
        topic: "held-turn".into(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    };
    ctx.reentry_tx.send(event).unwrap();
    sip_clock::testkit::settle().await;
    assert!(ctx.dispatcher.has_queue(&r), "the turn waits on the lock");
    ctx.dispatcher.release(&r, RemovalClass::SelfRelease);

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(n.cdr.snapshot().is_empty(), "the ghost waits for the lock");
    drop(held);
    ended_once(&n, &r).await;
}
