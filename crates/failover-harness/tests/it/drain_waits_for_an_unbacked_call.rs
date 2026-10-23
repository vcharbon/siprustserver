//! **A drain never reads an unbacked call as held** (ADR-0055 §3, ADR-0031
//! D2). The call's backup refused its replica at its ceiling, so the call has
//! no backup for life and its backup's flow reads caught up all the same. The
//! caught-up exit is an exit on evidence that a peer holds every live call:
//! with an unbacked call live, there is none, and the drain waits for the call
//! or the grace, as a non-HA B2BUA would.
//!
//! ```text
//!   established · the backup refuses the replica · the flow reads caught up
//!   t0         the primary is withdrawn from routing and drains
//!   t0..grace  no caught-up exit while the unbacked call lives
//!   then       the endpoint is readmitted and the call ends through its
//!              primary; one CDR, nothing left
//! ```

use std::time::Duration;

use b2bua::capacity::BackupCeilings;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, DrainBounds, DrainExit,
    FailoverHarness, Partition, ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const GRACE: Duration = Duration::from_secs(5);
const BOUNDS: DrainBounds = DrainBounds {
    grace: GRACE,
    floor: Duration::from_millis(1000),
    release_flush: Duration::from_secs(3),
    cdr_flush: Duration::ZERO,
};

#[tokio::test(start_paused = true)]
async fn a_drain_waits_for_an_unbacked_call() {
    let replica = b2bua::ReplicaConfig {
        backup: BackupCeilings { calls: Some(0), rss_bytes: None },
        ..Default::default()
    };
    let mut fh = FailoverHarness::new("drain-waits-for-an-unbacked-call", &["b1", "b2"])
        .with_worker_replica(replica);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── an established call whose backup refused its replica ─────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (primary, backup): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let call_ref = primary.scan_primary(&pri_ord).pop().expect("the primary stores the call");
    assert!(backup.scan_backed_up(&pri_ord).is_empty(), "the backup refused the replica");
    assert!(primary.flow_caught_up(&bak_ord, Partition::Bak), "the backup's flow reads caught up");

    // ── the endpoint is withdrawn; the drain finds no peer holding the call ───
    let t0 = fh.now_ms();
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(primary, BOUNDS);
    while fh.now_ms() - t0 < GRACE.as_millis() as i64 {
        let early = drain.poll();
        assert!(early.is_none(), "no exit while the unbacked call lives, got {early:?}");
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
    assert_ne!(outcome.exit, DrainExit::CaughtUp, "an unbacked call is never read as held");
    assert_eq!(outcome.residual, 1, "the live call is reported");
    assert_eq!(primary.metrics().drain_exits("caught_up"), 0);

    // ── the endpoint comes back; the call ends through its primary ──────────
    fh.readmit(&pri_ord, primary.sip_addr());
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
