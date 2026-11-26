//! **An endpoint set emptied at once withdraws no worker (ADR-0031 D6).**
//!
//! The orchestrator deletes and recreates the Service: its endpoint set
//! vanishes, every worker's own endpoint included, then comes back with the
//! same processes behind it. No member was singled out, so no worker reads
//! its own withdrawal: none latches Draining, the withdrawn-but-running gauge
//! stays off, each keeps serving what it holds, and once the set is listed
//! again the call ends on its primary as if nothing happened.
//!
//! ```text
//!   established · replicated · both workers routable
//!   t0          the slice empties: the proxy and both workers' views are
//!               reconciled to the empty set, the processes are untouched
//!   t0..t0+6 s  the empty view outlives a periodic membership reconcile (5 s)
//!   t0+6 s      the slice fills again: the same incarnations, the same addresses
//!   alice ──BYE──▶ proxy ──▶ primary ──▶ 200
//! ```
//!
//! Each removal is applied in one poll of the paused current-thread runtime,
//! so every worker reads the empty set and never a partial one.
//!
//! Beyond the failover sweep: a whole-set removal is a control-plane event, not
//! a withdrawal.

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Belief, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// How long the slice stays empty: past one periodic membership reconcile.
const EMPTY_FOR: Duration = Duration::from_secs(6);

#[tokio::test(start_paused = true)]
async fn an_endpoint_set_emptied_at_once_withdraws_no_worker() {
    let mut fh = FailoverHarness::new("endpoint-set-emptied-at-once", &["b1", "b2"]);

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

    // ── an established, replicated call on whichever worker the cookie picked ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (primary, backup): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let call_ref =
        backup.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");
    assert!(primary.serves(&call_ref), "the primary serves the call");

    // ── the whole endpoint set leaves the slice at once, and stays out ────────
    fh.empty_the_slice();
    fh.advance(EMPTY_FOR).await;
    for w in [&w_b1, &w_b2] {
        let ord = w.ordinal();
        assert!(!w.is_withdrawn(), "{ord} read a control-plane event as its own withdrawal");
        assert!(!w.is_draining(), "{ord} latched Draining on an emptied slice");
        assert!(!w.metrics().withdrawn_running(), "{ord} is not withdrawn-but-running");
    }
    assert!(primary.serves(&call_ref), "the primary keeps serving the call");
    assert!(!backup.serves(&call_ref), "the backup took nothing over from a running primary");

    // ── the recreated Service lists the same endpoints again ──────────────────
    fh.fill_the_slice();
    for ord in ["b1", "b2"] {
        proxy.set_health(ord, WorkerHealth::Alive);
    }
    fh.advance(Duration::from_secs(1)).await;
    for w in [&w_b1, &w_b2] {
        let ord = w.ordinal();
        assert!(w.is_ready(), "{ord} is ready once the slice lists it again");
        assert!(!w.is_withdrawn() && !w.is_draining(), "{ord} never withdrew");
    }

    // ── the call ends on its primary, cleanly ─────────────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let nodes: [&ReplicatedB2buaSut; 2] = [&w_b1, &w_b2];
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(&call_ref).await
                && !nodes[1].holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(&nodes[..], &call_ref).await;

    // ── what the ledger recorded: neither worker ever called itself draining ──
    let ledger = fh.view_ledger();
    for ord in ["b1", "b2"] {
        let own_view = ledger.beliefs(&format!("{ord}#g1"), ord);
        assert_eq!(
            own_view,
            vec![Belief::Running { gen: 1 }],
            "{ord} ran throughout: {own_view:?}",
        );
    }

    drop((w_b1, w_b2, proxy));
}
