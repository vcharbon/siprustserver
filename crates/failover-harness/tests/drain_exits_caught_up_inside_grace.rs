//! **A withdrawn worker's drain exits as soon as its backup holds every live
//! call it serves (ADR-0031 D2).**
//!
//! The orchestrator withdraws the endpoint from routing and leaves it in the
//! slice as terminating, so the peer keeps pulling it (D1). Nothing new can
//! arrive, the peer's Backup flow reports it has applied the worker's changelog
//! head, and the drain returns on that evidence — once the floor a request
//! routed just before the withdrawal needs has passed, and far inside the
//! grace. The residual live call is not lost but handed over: the endpoint
//! leaves the slice, the process exits, and a replacement of the same ordinal
//! reclaims the call and carries it to its end.
//!
//! ```text
//!   established · elder serves · the survivor's Backup flow is at the head
//!   t0        the endpoint reads terminating: the proxy departs it, the
//!             survivor keeps pulling it, the elder latches Draining and drains
//!   t0+1 s    the floor passes with the flow at the head ⇒ exit caught_up
//!   then      the endpoint leaves the slice, the process exits, a replacement
//!             of the same ordinal bootstraps and reclaims the call
//!   alice ──BYE──▶ proxy ──▶ replacement ──▶ 200
//! ```

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Belief, DrainBounds, DrainExit,
    FailoverHarness, Partition, PeerLink, ReplicatedB2buaSut, WorkerHealth,
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
const FLOOR: Duration = Duration::from_millis(1000);
const BOUNDS: DrainBounds = DrainBounds { grace: GRACE, floor: FLOOR };

#[tokio::test(start_paused = true)]
async fn a_withdrawn_workers_drain_exits_on_its_backup_holding_the_call() {
    let mut fh = FailoverHarness::new("drain-exits-caught-up-inside-grace", &["b1", "b2"]);

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
    assert!(
        elder.flow_caught_up(&bak_ord, Partition::Bak),
        "the backup's flow has reported applying everything logged for it"
    );

    // ── the endpoint is withdrawn from routing and the drain begins ───────────
    let t0 = fh.now_ms();
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(elder, BOUNDS);
    assert!(proxy.health(&pri_ord).is_none(), "the withdrawn ordinal no longer resolves");

    // The floor is waited out whatever the peers report: a request the proxy
    // routed just before the withdrawal is still owed its service.
    while fh.now_ms() - t0 < FLOOR.as_millis() as i64 {
        let early = drain.poll();
        assert!(
            early.is_none(),
            "the caught-up exit never fires before the floor, got {early:?} at +{} ms",
            fh.now_ms() - t0
        );
        fh.advance(Duration::from_millis(100)).await;
    }
    let mut outcome = None;
    for _ in 0..30 {
        if let Some(o) = drain.poll() {
            outcome = Some(o);
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned");

    // ── what the drain returned, and on what evidence ────────────────────────
    assert_eq!(
        outcome.exit,
        DrainExit::CaughtUp,
        "the drain exited on its backup holding the call, in {:?}",
        outcome.elapsed
    );
    assert_eq!(outcome.residual, 1, "the live call is handed to its backup, not lost");
    assert!(outcome.elapsed >= FLOOR, "the floor is waited out, got {:?}", outcome.elapsed);
    assert!(
        outcome.elapsed < Duration::from_secs(2),
        "and the exit is far inside the grace, got {:?}",
        outcome.elapsed
    );
    assert_eq!(elder.metrics().drain_exits("caught_up"), 1, "the exit reason is on the counter");
    assert_eq!(
        elder.metrics().drain_exits("grace_peers_behind"),
        0,
        "no flush window was lost, so nothing is reported as one"
    );
    assert!(
        elder.flow_caught_up(&bak_ord, Partition::Bak),
        "the predicate the exit read still holds when it returns"
    );
    assert!(elder.serves(&call_ref), "the drain does not stand the process down (D5)");
    assert!(elder.is_withdrawn() && elder.is_draining(), "it drained as a withdrawn worker");

    // D1 kept the survivor pulling right through the drain: that is what let the
    // elder's last flushes land and its Backup flow report the head.
    assert_eq!(
        survivor.peer_link(&pri_ord),
        PeerLink::Kept,
        "the survivor keeps pulling the terminating member"
    );
    let survivor_key = format!("{}#g1", survivor.ordinal());
    let survivor_view = fh.view_ledger().beliefs(&survivor_key, &pri_ord);
    assert!(
        !survivor_view.contains(&Belief::PeerParked),
        "nothing parked the peer through the drain: {survivor_view:?}"
    );

    // ── the pod is gone: the endpoint leaves the slice, the process exits ─────
    fh.depart(&pri_ord);
    fh.mark(&pri_ord, None, "crash", "the drained process exits");
    elder.crash();
    fh.advance(Duration::from_millis(200)).await;
    assert_eq!(
        survivor.peer_link(&pri_ord),
        PeerLink::Parked,
        "leaving the slice is what parks the peer"
    );

    // ── a replacement of the SAME ordinal reclaims the abandoned call ─────────
    let replacement = fh.spawn_replacement(&pri_ord).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            replacement.is_ready() && replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement bootstrapped and reclaimed the handed-over call");
    fh.mark(
        &pri_ord,
        None,
        "reclaimed",
        &format!("the replacement holds the call at +{} ms", fh.now_ms() - t0),
    );
    fh.readmit(&pri_ord, replacement.sip_addr());
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;

    // ── the call ends on the replacement, cleanly ────────────────────────────
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
        "running, then draining, then dead once the drain returned: {elder_view:?}",
    );

    drop((w_b1, w_b2, replacement, proxy));
}
