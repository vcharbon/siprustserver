//! **A withdrawn worker whose backup reports nothing drains to the grace, and
//! says so (ADR-0031 D2).**
//!
//! The caught-up exit is an exit on *evidence*: the peer that holds the other
//! copy of every live call has reported applying this worker's changelog head.
//! Here the peer parks its flows to the terminating member instead of pulling it
//! through its drain (what D1 forbids, and what a membership lag or a restart of
//! the peer produces), so no flow carries the worker's Backup sub-log and
//! nothing can report a position. The worker has no proof its calls survive it:
//! it waits out the whole grace and returns `grace_peers_behind` — a lost flush
//! window, never a clean drain.
//!
//! The peer's copy is current all along: what the grace exit reports is missing
//! evidence, not missing data. The flow comes back when the peer re-adds the
//! member, the predicate holds again — it is not latched — and the abandoned call
//! is reclaimed from the peer by a replacement of the same ordinal and terminated
//! properly.
//!
//! ```text
//!   established · elder serves · survivor holds a current replica
//!   t0-        the survivor parks its flows to the elder: no flow carries the
//!              elder's Backup sub-log for it
//!   t0         the endpoint reads terminating; the elder latches Draining and drains
//!   t0+5 s     the grace ends with nothing reported ⇒ exit grace_peers_behind
//!   then       the flow returns, the endpoint leaves the slice, the process exits
//!              and a replacement of the same ordinal reclaims the call
//!   alice ──BYE──▶ proxy ──▶ replacement ──▶ 200
//! ```

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Belief, DrainBounds, DrainExit,
    FailoverHarness, Partition, ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The deployed shutdown bounds: the 5 s ceiling with the caught-up exit's 1 s
/// floor (ADR-0031 D2).
const GRACE: Duration = Duration::from_secs(5);
const BOUNDS: DrainBounds = DrainBounds { grace: GRACE, floor: Duration::from_millis(1000) };

#[tokio::test(start_paused = true)]
async fn a_withdrawn_worker_whose_backup_reports_nothing_drains_to_the_grace() {
    let mut fh = FailoverHarness::new("drain-waits-for-a-backup-behind", &["b1", "b2"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── an established, replicated call on whichever worker the cookie picked ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (elder, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    let call_ref =
        survivor.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");
    assert!(elder.serves(&call_ref), "the primary serves the call");
    assert!(elder.flow_caught_up(&bak_ord, Partition::Bak), "the backup's flow is at the head");

    // ── the survivor parks its flows to the member the orchestrator is removing ─
    fh.mark(&bak_ord, Some(&pri_ord), "park", "the peer drops the terminating member early");
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;
    assert!(
        !elder.flow_caught_up(&bak_ord, Partition::Bak),
        "no flow carries the backup sub-log, so nothing can report a position"
    );
    assert!(
        survivor.is_synchronized_backup(&call_ref).await,
        "the peer's copy is current all the same: the missing thing is the evidence"
    );

    // ── the endpoint is withdrawn from routing and the drain begins ───────────
    let t0 = fh.now_ms();
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(elder, BOUNDS);
    assert!(proxy.health(&pri_ord).is_none(), "the withdrawn ordinal no longer resolves");

    // Nothing returns early: with no peer reporting the call, the grace is the
    // only bound left — the floor is not an exit, it is a minimum.
    while fh.now_ms() - t0 < GRACE.as_millis() as i64 {
        let early = drain.poll();
        assert!(
            early.is_none(),
            "no exit while no peer reports holding the call, got {early:?} at +{} ms",
            fh.now_ms() - t0
        );
        fh.advance(Duration::from_millis(100)).await;
    }
    let mut outcome = None;
    for _ in 0..20 {
        if let Some(o) = drain.poll() {
            outcome = Some(o);
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned");

    // ── what the drain returned: a lost flush window, named ──────────────────
    assert_eq!(
        outcome.exit,
        DrainExit::GracePeersBehind,
        "a withdrawn worker reaching its grace lost a flush window, in {:?}",
        outcome.elapsed
    );
    assert_eq!(outcome.residual, 1, "the abandoned call is reported, never silently cut");
    assert!(outcome.elapsed >= GRACE, "it waited the whole grace, got {:?}", outcome.elapsed);
    assert_eq!(
        elder.metrics().drain_exits("grace_peers_behind"),
        1,
        "the reason is on the counter"
    );
    assert_eq!(
        elder.metrics().drain_exits("caught_up"),
        0,
        "and it is never counted as a clean hand-over"
    );
    assert!(elder.serves(&call_ref), "the drain does not stand the process down (D5)");
    assert!(elder.is_withdrawn() && elder.is_draining(), "it drained as a withdrawn worker");

    // ── the peer re-adds the member: the predicate is not latched ────────────
    survivor.simulate_peer_added(&pri_ord);
    let resynced = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            elder.flow_caught_up(&bak_ord, Partition::Bak)
        })
        .await;
    assert!(resynced, "a returning flow reports the head again");

    // ── the pod is gone: the endpoint leaves the slice, the process exits ─────
    fh.depart(&pri_ord);
    fh.mark(&pri_ord, None, "crash", "the process exits with its calls still live");
    elder.crash();
    fh.advance(Duration::from_millis(300)).await;

    // ── the call the drain gave up on was held after all ─────────────────────
    // The peer's copy was current the whole time, so a replacement of the same
    // ordinal reclaims it and can carry it to its end.
    let replacement = fh.spawn_replacement(&pri_ord).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            replacement.is_ready() && replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement bootstrapped and reclaimed the abandoned call");
    fh.readmit(&pri_ord, replacement.sip_addr());
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let nodes: [&ReplicatedB2buaSut; 3] = [&*elder, survivor, &replacement];
    let drained = fh
        .settle_terminal(async || {
            let mut clean = true;
            for n in nodes {
                if !n.memory_clean() || n.holds_any_trace(&call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(&nodes[..], &call_ref).await;

    // ── what the ledger recorded of the drained incarnation ──────────────────
    let elder_view = fh.view_ledger().beliefs(&format!("{pri_ord}#g1"), &pri_ord);
    assert_eq!(
        elder_view,
        vec![Belief::Running { gen: 1 }, Belief::Draining { gen: 1 }, Belief::Dead { gen: 1 }],
        "running, then draining, then dead at the end of its grace: {elder_view:?}",
    );

    drop((w_b1, w_b2, replacement, proxy));
}
