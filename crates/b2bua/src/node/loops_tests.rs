//! The core's two loops, the transaction owner and the router: an idle core
//! never reads stalled, a minute idle against a 5 s threshold; a router turn
//! held past the threshold reads stalled; a bulk reclaim meeting held call
//! locks does not, since it waits on none; a crashed core's loops read ended
//! within one supervision poll.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_ids::TurnIds;
use sip_net_testkit::SimulatedSignalingNetwork;
use tokio::sync::watch;

use super::unwired_tests::unwired_core;
use crate::config::B2buaConfig;
use crate::decision::ScriptedDecisionEngine;
use crate::initial_invite::build_initial_call;
use crate::loops::{LoopOut, ENDED_POLL};
use crate::router::test_support::{invite, node, node_probed, src, Node};
use crate::router::ReplCommand;
use crate::store::{CallStore, PartitionRole, PutOpts};
use crate::turn_probe::{Hold, TurnMark, TurnProbe, TurnProbes};

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
    let out = tokio::time::timeout(ENDED_POLL * 2, loops.wait_out(THRESHOLD)).await;
    assert!(matches!(out, Ok(LoopOut::Ended(_))), "a loop task ended: {out:?}");
}

/// Put a ringing call of this node's own into its `pri:` partition, not live:
/// what a reboot reclaims.
async fn seeded(n: &Node, cid: &str) -> String {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let call =
        build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut TurnIds::fixed(1), 0);
    let body = crate::store::envelope::placed_body(&call).unwrap();
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

/// Holds the router loop's next turn once armed, until the test lets it go.
struct HoldTurn {
    armed: AtomicBool,
    go: watch::Receiver<bool>,
}

impl TurnProbe for HoldTurn {
    fn reached(&self, _mark: &TurnMark<'_>) -> Option<Hold> {
        None
    }

    fn router_turn(&self, _node: &str) -> Option<Hold> {
        if !self.armed.swap(false, Ordering::SeqCst) {
            return None;
        }
        let mut go = self.go.clone();
        Some(Box::pin(async move {
            let _ = go.wait_for(|go| *go).await;
        }))
    }
}

/// A router turn held past the threshold reads stalled, and turns again once
/// it is let go.
#[tokio::test(start_paused = true)]
async fn a_router_turn_held_past_the_threshold_reads_stalled() {
    let (go, held) = watch::channel(false);
    let hold = Arc::new(HoldTurn { armed: AtomicBool::new(false), go: held });
    let net = SimulatedSignalingNetwork::new(1);
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.2", 9));
    let probes = TurnProbes::new(hold.clone());
    let n = node_probed(&net, "w0", decision, |_| {}, Vec::new(), probes).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    hold.armed.store(true, Ordering::SeqCst);
    send(&n, ReplCommand::ReclaimAll);
    tokio::time::sleep(THRESHOLD).await;
    assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "at the threshold, not past it");
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(n.core.loops().stalled(THRESHOLD), ["router"]);
    go.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "the turn ended");
}

/// A bulk reclaim waits on no call's lock: calls whose locks turns hold well
/// past the threshold leave the router turning, and are reclaimed on turns of
/// their own once their locks are free.
#[tokio::test(start_paused = true)]
async fn a_bulk_reclaim_meeting_held_call_locks_never_reads_stalled() {
    let n = node("w0").await;
    let mut refs = Vec::new();
    for i in 0..3 {
        refs.push(seeded(&n, &format!("bulk-{i}")).await);
    }
    let mut held = Vec::new();
    for call_ref in &refs {
        held.push(n.core.ctx.state.lock(call_ref).await);
    }
    send(&n, ReplCommand::ReclaimAll);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(n.core.loops().stalled(THRESHOLD).is_empty(), "the router waits on no lock");
    }
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(n.metrics.repl_reclaimed_total(), 3, "every call reclaimed");
    for call_ref in &refs {
        assert!(n.core.ctx.state.contains(call_ref), "reclaimed: {call_ref}");
        assert!(!n.core.ctx.state.is_parked(call_ref), "and armed: {call_ref}");
    }
}
