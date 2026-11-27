//! **A call that ends between two cold bootstrap passes leaves no copy on the
//! node that bootstraps** (ADR-0014 §8). A returning node's first pass of a
//! flow carries call X, then the stream drops before the pass's catch-up
//! `Noop`, so the node's watermark stays cold. X ends on the node that holds it
//! while the stream is down. The second pass is cold again: its scan no longer
//! lists X when X's record is gone, and its tail starts past X's delete, so
//! neither carries the end. Once that pass catches up, the bootstrapping node
//! holds exactly the live set of the flow and X stays over: no node ends it a
//! second time, no request of X leaves a node, X has its one CDR.
//!
//! ```text
//!   Backup flow (b2 backs b1's calls up)
//!   b2 restarts in place     b1 answers X and Y, each w_bak=b2
//!   b2 reboots               b1 → b2 delayed 500 ms a frame: pass 1 lands X
//!   b2 holds X, no Noop      b1 ✂ b2: pass 1 dropped, b2's watermark cold
//!   alice ─BYE(X)─▶ b1       X ends on b1, its CDR there, its record deleted
//!   heal                     pass 2: the scan carries Y, the tail starts past
//!                            X's delete; its first Noop: b2 caught up, ready
//!   b2 holds Y alone
//!   b1 dies and leaves       alice ─BYE(Y)─▶ b2: served, one CDR
//!   past the replica TTL     X: no request from b2, still one CDR
//!
//!   Reclaim flow (b2 recovers its own calls from b1's copies)
//!   b2 answers X and Y, each w_bak=b1
//!   b2 restarts in place     │ b2 departs, b1 not told it is back
//!   b2 reboots               b1 → b2 delayed 500 ms a frame: pass 1 lands X
//!   b2 holds X, no Noop      b1 ✂ b2
//!   alice ─BYE(X)─▶ b1       b1's copy takes X over and ends it, keeping X's
//!                            ended record for b2's reclaim
//!                            │ b2 departed: the copy discharges X, its CDR
//!                            │ there, its record deleted
//!   heal                     pass 2 carries Y (and X's ended record while b1
//!                            keeps it); its first Noop reclaims
//!   b2 serves Y alone, holds no trace of X
//! ```
//!
//! Timing axes: each bootstrap frame lands one 500 ms delay after the one
//! before it, so pass 1 is cut within 100 ms of X landing, at least 400 ms
//! before its next frame. X ends while the cut holds, 500 ms before the heal
//! and well inside the puller's 10 s hard timer; pass 2 starts at the heal and
//! catches up on its own `Noop` within a few hundred ms. In the cell whose
//! backup's view loses the primary, the primary leaves that view right after
//! the cut and returns once both flows read parked (within 100 ms), before X
//! ends. Every step is 100 ms or more apart, so no verdict depends on in-order
//! draining.
//!
//! Beyond the failover sweep: a call ended between two cold bootstrap passes stays over.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    total_cdrs_for, FailoverHarness, Partition, PeerLink, ProxySut, ReplicatedB2buaSut,
    WorkerHealth,
};
use repl_net::frame::{Frame, Watermark};
use repl_net_testkit::Direction;
use scenario_harness::Agent;

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, pod_deleted, restart_in_place, settle, Answered, ALICE,
    BOB,
};

/// How late each frame from b1 reaches the rebooted b2: one bootstrap body per
/// delay, under the puller's 2 s idle bound.
const FRAME_DELAY_MS: u64 = 500;

/// Past the replica's TTL (`reboot_budget`, 600 s), the reap that follows it
/// (every 30 s) and one keepalive interval (300 s): whatever a copy left
/// behind would have probed, ended or been reaped by then.
const PAST_THE_REPLICA_TTL: Duration = Duration::from_secs(720);

/// Two workers behind the proxy and the parties: `holder` is the node the
/// `restarted` one bootstraps its flow from.
struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    holder: ReplicatedB2buaSut,
    restarted: ReplicatedB2buaSut,
}

/// The cluster once the second pass has caught up: X ended between the
/// passes, Y lives on.
struct Passed {
    cluster: Cluster,
    x: Answered,
    y: Answered,
}

/// How `restarted` comes back for its two passes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Return {
    /// Restarted in place: its peers keep it in their set.
    InPlace,
    /// Departed and back at its ordinal, seen by its own informer only: its
    /// peers still read it departed.
    UnseenByPeers,
}

/// What the bootstrapping node's view of its holder does while pass 1's cut
/// holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InTheCut {
    /// The view keeps the holder: pass 2 runs on the puller pass 1 ran on.
    Kept,
    /// The holder leaves the view and returns at the same incarnation and
    /// address: the flow is parked, and pass 2 runs on a puller spawned anew.
    LeftAndReturned,
}

/// Every cold `PullRequest` `caller` sent on `flow`, as the local end of its
/// stream.
fn cold_pulls(fh: &FailoverHarness, caller: &str, flow: Partition) -> Vec<SocketAddr> {
    fh.repl_report()
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Sent)
        .filter_map(|f| match &f.frame {
            Frame::PullRequest { caller: c, partition, since, .. }
                if c == caller && *partition == flow && *since == Watermark::new(0, 0) =>
            {
                Some(f.from)
            }
            _ => None,
        })
        .collect()
}

/// How many `Noop`s the stream whose local end is `local` received.
fn noops_on(fh: &FailoverHarness, local: SocketAddr) -> usize {
    fh.repl_report()
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Received && f.from == local)
        .filter(|f| matches!(f.frame, Frame::Noop { .. }))
        .count()
}

/// The requests `node` sent inside a dialog of `call`, toward either party.
fn requests_of(fh: &FailoverHarness, node: SocketAddr, call: &Answered) -> usize {
    let tags = [call.dialog.local_tag(), call.bob_dialog.local_tag()];
    fh.sip_entries()
        .iter()
        .filter(|e| e.from == node && !e.opens_with("SIP/2.0"))
        .filter(|e| {
            let raw = String::from_utf8_lossy(&e.raw);
            tags.iter().any(|t| raw.contains(t))
        })
        .count()
}

/// The proxy and both workers ready, the parties bound.
async fn cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, holder, restarted) = bring_up(&mut fh).await;
    Cluster { fh, alice, bob, proxy, holder, restarted }
}

/// Two calls answered on `on`, each signed by the proxy with `ordinals`
/// (primary, backup).
async fn two_calls(c: &Cluster, on: &ReplicatedB2buaSut, ordinals: (&str, &str)) -> [Answered; 2] {
    let calls = [
        answered_on(&c.fh, &c.alice, &c.bob, &c.proxy, on).await,
        answered_on(&c.fh, &c.alice, &c.bob, &c.proxy, on).await,
    ];
    let want = (ordinals.0.to_string(), ordinals.1.to_string());
    for call in &calls {
        assert_eq!(call.cookie, want, "{} serves, {} backs up", ordinals.0, ordinals.1);
    }
    calls
}

/// `holder` leaves `node`'s view, which parks both flows toward it, and
/// returns once they read parked, which spawns their pullers anew.
async fn leaves_and_returns(fh: &mut FailoverHarness, node: &ReplicatedB2buaSut, holder: &str) {
    fh.mark(node.ordinal(), Some(holder), "peer_removed", "the holder leaves this node's view");
    node.simulate_peer_removed(holder);
    let parked = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(1), async || {
            node.peer_link(holder) == PeerLink::Parked
        })
        .await;
    assert!(parked, "{} parks its flows toward {holder}", node.ordinal());
    fh.mark(node.ordinal(), Some(holder), "peer_returned", "the same incarnation and address");
    node.simulate_peer_returned(holder);
    let back = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(1), async || {
            node.peer_link(holder) != PeerLink::Parked
        })
        .await;
    assert!(back, "{} pulls {holder} again", node.ordinal());
}

/// `restarted` reboots under the delay, its first cold pass of `flow` is cut
/// once it holds one of `calls`, its view of the holder does `in_the_cut`,
/// that call (X) ends through the proxy while the cut holds, and the second
/// pass catches up.
async fn two_passes(
    mut c: Cluster,
    flow: Partition,
    mut calls: [Answered; 2],
    back: Return,
    in_the_cut: InTheCut,
) -> (Passed, SocketAddr) {
    let (h, r) = (c.holder.ordinal().to_string(), c.restarted.ordinal().to_string());
    let earlier = cold_pulls(&c.fh, &r, flow).len();
    c.fh.delay(&h, &r, FRAME_DELAY_MS);
    c.fh.mark(&r, None, "reboot", "a fresh incarnation at a new address");
    let addr = c.restarted.reboot().await;
    c.fh.note_worker_rebound(&r, addr);
    if back == Return::UnseenByPeers {
        c.fh.publish_to_itself(&r);
    }

    let held = |n: &ReplicatedB2buaSut| match flow {
        Partition::Bak => n.scan_backed_up(&h),
        Partition::Pri => n.scan_primary(&r),
    };
    let landed =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            !held(&c.restarted).is_empty()
        })
        .await;
    assert!(landed, "pass 1 carried a call");
    let pass1 = cold_pulls(&c.fh, &r, flow).split_off(earlier);
    assert_eq!(pass1.len(), 1, "one cold pass since the reboot");
    assert_eq!(noops_on(&c.fh, pass1[0]), 0, "pass 1 has not caught up when it is cut");
    let first = held(&c.restarted);
    assert_eq!(first.len(), 1, "pass 1 landed one call before the cut: {first:?}");
    let x = calls.iter().position(|call| call.call_ref == first[0]).expect("pass 1 landed X or Y");
    c.fh.cut_streams(&h, &r);
    if in_the_cut == InTheCut::LeftAndReturned {
        leaves_and_returns(&mut c.fh, &c.restarted, &h).await;
    }

    caller_hangs_up(&mut calls[x].dialog, &c.bob).await;
    c.fh.advance(Duration::from_millis(500)).await;
    let x_ref = &calls[x].call_ref;
    let left_to_the_reclaim = flow == Partition::Pri && back == Return::InPlace;
    assert_eq!(
        total_cdrs_for(&[&c.holder], x_ref),
        usize::from(!left_to_the_reclaim),
        "X ended on {h}: its CDR there, unless its copy leaves the end to the primary's reclaim"
    );
    let record_kept = match flow {
        Partition::Bak => c.holder.scan_primary(&h),
        Partition::Pri => c.holder.scan_backed_up(&r),
    }
    .contains(x_ref);
    assert_eq!(
        record_kept, left_to_the_reclaim,
        "{h} keeps X's ended record only as the copy of a primary it reads alive"
    );
    c.fh.heal(&h, &r);

    let ready =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            c.restarted.is_ready()
        })
        .await;
    assert!(ready, "pass 2 caught up: {r} is ready");
    let pulls = cold_pulls(&c.fh, &r, flow).split_off(earlier);
    assert!(pulls.len() >= 2, "pass 2 pulled cold again: {pulls:?}");
    let caught_up =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(2), async || {
            pulls.last().is_some_and(|last| noops_on(&c.fh, *last) > 0)
        })
        .await;
    assert!(caught_up, "pass 2 caught up on its own Noop, not on the hard timer");

    let [c0, c1] = calls;
    let (x, y) = if x == 0 { (c0, c1) } else { (c1, c0) };
    (Passed { cluster: c, x, y }, addr)
}

/// `restarted` is published at `addr` to the proxy and its peers and read
/// alive, and `holder` reads its `flow` caught up.
async fn published(c: &mut Cluster, flow: Partition, addr: SocketAddr) {
    let r = c.restarted.ordinal().to_string();
    c.fh.readmit(&r, addr);
    c.proxy.set_health(&r, WorkerHealth::Alive);
    let caught_up =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(2), async || {
            c.holder.flow_caught_up(&r, flow)
        })
        .await;
    assert!(caught_up, "{} reads {r}'s flow caught up once {r} is published", c.holder.ordinal());
}

/// Backup flow: b1 answers X and Y while b2 is away; b2 bootstraps them twice,
/// X ending on b1 in between, b2's view of b1 doing `in_the_cut`.
async fn backup_passes(name: &str, in_the_cut: InTheCut) -> Passed {
    let mut c = cluster(name).await;
    restart_in_place(&mut c.fh, &mut c.restarted);
    c.fh.advance(Duration::from_millis(500)).await;
    let calls = two_calls(&c, &c.holder, ("b1", "b2")).await;
    let (mut passed, addr) =
        two_passes(c, Partition::Bak, calls, Return::InPlace, in_the_cut).await;
    published(&mut passed.cluster, Partition::Bak, addr).await;
    passed
}

/// Reclaim flow: b2 answers X and Y, b1 backing them up, and b2 dies the way
/// `back` returns; b2 recovers them twice, X ending on b1's copy in between.
async fn reclaim_passes(name: &str, back: Return) -> Passed {
    let mut c = cluster(name).await;
    c.fh.flap_not_ready("b1");
    c.fh.advance(Duration::from_millis(500)).await;
    let calls = two_calls(&c, &c.restarted, ("b2", "b1")).await;
    c.fh.flap_ready("b1");
    c.proxy.set_health("b1", WorkerHealth::Alive);
    let backed =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            let held = c.holder.scan_backed_up("b2");
            calls.iter().all(|call| held.contains(&call.call_ref))
        })
        .await;
    assert!(backed, "b1 holds both calls: {:?}", c.holder.scan_backed_up("b2"));

    match back {
        Return::InPlace => restart_in_place(&mut c.fh, &mut c.restarted),
        Return::UnseenByPeers => pod_deleted(&mut c.fh, &mut c.restarted),
    }
    c.fh.advance(Duration::from_millis(500)).await;
    let (mut passed, addr) = two_passes(c, Partition::Pri, calls, back, InTheCut::Kept).await;
    published(&mut passed.cluster, Partition::Pri, addr).await;
    passed
}

/// After the second pass, the backup holds the primary's live set, Y and not
/// X, whatever puller the second pass runs on; both calls end with one CDR
/// each.
async fn holds_the_live_set(name: &str, in_the_cut: InTheCut) {
    let Passed { cluster: c, x, mut y } = backup_passes(name, in_the_cut).await;

    assert_eq!(
        c.restarted.scan_backed_up("b1"),
        vec![y.call_ref.clone()],
        "b2 holds b1's live set once caught up: Y, live through both passes, and no copy of X"
    );

    caller_hangs_up(&mut y.dialog, &c.bob).await;
    let refs = [x.call_ref.as_str(), y.call_ref.as_str()];
    settle(&c.fh, &[&c.holder, &c.restarted], &refs, &[&c.alice, &c.bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&c.holder, &c.restarted], r), 1, "one CDR for {r}");
    }
    c.fh.assert_sip_rfc_clean(name);
}

/// The second pass runs on the puller the first one ran on.
#[tokio::test(start_paused = true)]
async fn a_backup_holds_no_copy_of_a_call_ended_between_its_two_bootstrap_passes() {
    holds_the_live_set("backup-copy-ended-between-two-passes", InTheCut::Kept).await;
}

/// The primary leaves the backup's view during the cut and returns: the
/// second pass runs on a puller spawned anew, which reads what the first
/// pass carried from the flow's record.
#[tokio::test(start_paused = true)]
async fn a_backup_whose_primary_left_its_view_between_two_passes_holds_no_copy_of_the_ended_call() {
    holds_the_live_set("backup-view-left-between-two-passes", InTheCut::LeftAndReturned).await;
}

/// The primary dies after the second pass: the backup serves Y with its one
/// CDR, and X stays over, past the replica's TTL.
#[tokio::test(start_paused = true)]
async fn a_takeover_after_two_bootstrap_passes_never_ends_the_call_ended_between_them() {
    let name = "takeover-after-two-passes";
    let Passed { cluster: mut c, x, mut y } = backup_passes(name, InTheCut::Kept).await;
    pod_deleted(&mut c.fh, &mut c.holder);
    c.proxy.set_health("b1", WorkerHealth::Dead);
    c.fh.advance(Duration::from_secs(1)).await;
    caller_hangs_up(&mut y.dialog, &c.bob).await;

    c.fh.pump_until(Duration::from_secs(30), PAST_THE_REPLICA_TTL, async || false).await;
    assert!(
        c.restarted.memory_clean()
            && !c.restarted.holds_any_trace(&x.call_ref).await
            && !c.restarted.holds_any_trace(&y.call_ref).await,
        "b2 releases both calls, past the replica's TTL"
    );
    c.fh.linger_peers(&[&c.alice, &c.bob], Duration::from_secs(3)).await;
    let nodes = [&c.holder, &c.restarted];
    assert_eq!(total_cdrs_for(&nodes, &y.call_ref), 1, "Y served by b2 with one CDR");
    assert_eq!(
        requests_of(&c.fh, c.restarted.sip_addr(), &x),
        0,
        "b2 sends no request inside X's dialogs"
    );
    assert_eq!(total_cdrs_for(&nodes, &x.call_ref), 1, "X keeps its one CDR, b1's");
    c.fh.assert_sip_rfc_clean(name);
}

/// The reclaiming node serves Y alone and holds no trace of X; both calls end
/// with one CDR each.
async fn reclaims_y_alone(name: &str, back: Return) {
    let Passed { cluster: c, x, mut y } = reclaim_passes(name, back).await;

    assert!(c.restarted.serves(&y.call_ref), "b2 reclaimed Y, live through both passes");
    assert!(!c.restarted.serves(&x.call_ref), "b2 does not serve X");
    assert!(!c.restarted.holds_any_trace(&x.call_ref).await, "b2 holds no trace of X");

    caller_hangs_up(&mut y.dialog, &c.bob).await;
    let refs = [x.call_ref.as_str(), y.call_ref.as_str()];
    settle(&c.fh, &[&c.holder, &c.restarted], &refs, &[&c.alice, &c.bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&c.holder, &c.restarted], r), 1, "one CDR for {r}");
    }
    c.fh.assert_sip_rfc_clean(name);
}

/// Restarted in place: the backup's copy that ended X keeps X's record, so
/// the second pass carries X's end.
#[tokio::test(start_paused = true)]
async fn a_reclaim_learns_the_end_of_a_call_taken_over_between_its_two_bootstrap_passes() {
    reclaims_y_alone("reclaim-in-place-across-two-passes", Return::InPlace).await;
}

/// Departed and back unseen: the backup's copy that ended X discharges it,
/// deleting X's record, so no pass carries X's end.
#[tokio::test(start_paused = true)]
async fn a_reclaim_holds_no_trace_of_a_call_discharged_between_its_two_bootstrap_passes() {
    reclaims_y_alone("reclaim-unseen-across-two-passes", Return::UnseenByPeers).await;
}
