//! The core's two loops, the transaction owner and the router: an idle core
//! never reads stalled, a minute idle against a 5 s threshold; a router turn
//! parked on one call reads stalled; a bulk reclaim making progress call by
//! call does not; a crashed core's loops read ended within one supervision
//! poll.

use std::time::Duration;

use call::{CallBodyCodec, MsgpackCodec};
use sip_ids::TurnIds;

use super::unwired_tests::unwired_core;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::loops::ENDED_POLL;
use crate::router::test_support::{invite, node, src, Node};
use crate::router::ReplCommand;
use crate::store::{CallStore, PartitionRole, PutOpts};

const THRESHOLD: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn an_idle_core_never_reads_stalled() {
    let core = unwired_core("w0").await;
    tokio::time::sleep(THRESHOLD * 12).await;
    assert!(core.loops().stalled(THRESHOLD).is_empty());
    assert_eq!(core.loops().ended(), None);
}

#[tokio::test(start_paused = true)]
async fn a_crashed_cores_loops_read_ended_within_one_poll() {
    let mut core = unwired_core("w0").await;
    let loops = core.loops().clone();
    core.abort();
    let ended = tokio::time::timeout(ENDED_POLL * 2, loops.wait_ended()).await;
    assert!(ended.is_ok(), "a loop task ended");
}

/// Put a ringing call of this node's own into its `pri:` partition, not live:
/// what a reboot reclaims.
async fn seeded(n: &Node, cid: &str) -> String {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let call =
        build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut TurnIds::fixed(1), 0);
    let body = MsgpackCodec::new().encode(&call).unwrap();
    n.store
        .put_call(
            PartitionRole::Primary,
            "w0",
            &call.call_ref,
            body,
            &[],
            600_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    call.call_ref
}

fn send(n: &Node, cmd: ReplCommand) {
    n.core._repl_tx.as_ref().expect("a wired node").send(cmd).unwrap();
}

/// A router turn waiting on one call's lock past the threshold reads stalled,
/// and turns again once the lock is free.
#[tokio::test(start_paused = true)]
async fn a_router_parked_on_one_event_reads_stalled() {
    let n = node("w0").await;
    let call_ref = seeded(&n, "parked").await;
    let held = n.core.ctx.state.lock(&call_ref).await;
    send(&n, ReplCommand::ReclaimCall(call_ref));
    tokio::time::sleep(THRESHOLD).await;
    assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "at the threshold, not past it");
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(n.core.loops().stalled(THRESHOLD), ["router"]);
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "the turn ended");
}

/// A bulk reclaim is one router turn over many calls: each call waiting under
/// the threshold keeps the router turning, however long the whole turn runs.
#[tokio::test(start_paused = true)]
async fn a_bulk_reclaim_of_calls_each_under_the_threshold_never_reads_stalled() {
    let n = node("w0").await;
    for i in 0..3 {
        seeded(&n, &format!("bulk-{i}")).await;
    }
    let order: Vec<String> = n
        .core
        .ctx
        .state
        .scan_partition(PartitionRole::Primary, "w0", &|| {})
        .await
        .into_iter()
        .map(|(call, _)| call.call_ref)
        .collect();
    let mut held = Vec::new();
    for call_ref in &order {
        held.push(n.core.ctx.state.lock(call_ref).await);
    }
    send(&n, ReplCommand::ReclaimAll);
    // The reclaim takes the calls in scan order: each lock is let go 3 s on.
    for guard in held {
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "the reclaim makes progress");
        }
        drop(guard);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(n.metrics.repl_reclaim_materialized(), 3, "every call reclaimed");
}
