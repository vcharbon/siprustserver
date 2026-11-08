//! The replica reap shares its task with the call reaper's sweep, whose
//! aborts free a hung turn: a ghost whose lock a turn holds is ended on a
//! turn of its own queue, and the reap never waits on that lock.

use std::time::Duration;

use crate::router::test_support::{answered, node_with};
use crate::store::{CallStore, PartitionRole, PutOpts};

#[tokio::test(start_paused = true)]
async fn a_ghost_reap_meeting_a_held_lock_never_stalls_the_reaper() {
    let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 1).await;
    let ctx = n.core.router_ctx();
    n.membership.add(topology::Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    // A call no node ended, its replica past its TTL, its primary departed.
    let ghost = answered("w0", "w1", "ghost");
    let r = ghost.call_ref.clone();
    let body = crate::store::envelope::placed_body(&ghost).unwrap();
    let keys = call::call_index_keys(&ghost);
    let opts = PutOpts::default();
    let bak = PartitionRole::Backup;
    n.store.put_call(bak, "w0", &r, body, &keys, 1_000, 1, 0, &opts).await.unwrap();
    n.membership.remove("w0");
    let held = ctx.state.lock(&r).await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    let sweeps = n.metrics.reaper_sweeps_total();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(n.metrics.reaper_sweeps_total() >= sweeps + 3, "the reaper sweeps on");
    assert!(n.cdr.snapshot().is_empty(), "the ghost waits for its turn");

    drop(held);
    sip_clock::testkit::settle().await;
    let cdrs: Vec<String> = n.cdr.snapshot().into_iter().map(|c| c.call_ref).collect();
    assert_eq!(cdrs, [r.clone()], "the ghost's one CDR, written on its turn");
    assert!(!ctx.state.contains(&r), "no copy is left");
    assert!(!ctx.dispatcher.has_queue(&r), "no queue is left behind");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}
