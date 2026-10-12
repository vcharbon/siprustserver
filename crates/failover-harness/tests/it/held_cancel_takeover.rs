//! **A CANCEL held when the serving node dies is sent by the survivor.** A
//! callee that has sent nothing has given the worker no provisional, so the
//! worker's CANCEL of it waits for one (RFC 3261 §9.1). The CANCEL is in the
//! replicated record; when the serving node dies before the callee answers,
//! the survivor rebuilds the INVITE transaction with that CANCEL held on it.
//! The callee's 100 never reaches the survivor: it carries no Record-Route
//! (§8.2.6.1), and the proxy's Via on an initial INVITE carries no cookie, so
//! the front proxy has none to fail it over on and drops it. The callee's 180
//! brings the Record-Route cookie back, reaches the survivor on it, and
//! releases the CANCEL (ADR-0043 X1).

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

#[tokio::test(start_paused = true)]
async fn a_cancel_held_at_the_kill_is_sent_by_the_survivor_on_the_callees_18x_not_its_100() {
    let mut fh = FailoverHarness::new("held-cancel-takeover", &["b1", "b2"]);
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

    // ── alice hears the worker's 100; bob says nothing yet ──────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    fh.advance(Duration::from_millis(500)).await;
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = backup.scan_one_backed_up(&pri_ord).await.expect("the call replicated");

    // ── alice gives up: answered at once, the b-leg CANCEL held, replicated ─
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "the serving node holds the CANCEL: bob gave it no provisional (§9.1)",
    );

    // ── the serving node dies with the CANCEL held ──────────────────────────
    let (primary, survivor) = if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    // ── bob's 100 dies at the proxy: no Record-Route, no cookie to follow ──
    uas.respond(100, "Trying").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "a 100 sent after the kill never reaches the survivor, so the CANCEL stays held",
    );

    // ── bob rings: the survivor takes the call over and sends the CANCEL ────
    uas.respond(180, "Ringing").await;
    bob.receive_absorbing("CANCEL", &["INVITE"]).await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;

    // ── the dead node reboots and reclaims; one CDR, everything released ────
    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the failed-over call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
}

/// Reboot the crashed primary empty at a higher gen + new pod IP and let its
/// go-active reclaim discharge the terminal the survivor deferred (ADR-0020 X3).
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) {
    let new_addr = primary.reboot().await;
    proxy.set_address(pri_ord, new_addr);
    fh.note_worker_rebound(pri_ord, new_addr);
    survivor.simulate_peer_added(pri_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {pri_ord} became ready");
    proxy.set_health(pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}
