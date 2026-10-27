//! **A worker whose ordinal is recreated while its process still runs stops
//! authoring at once** (ADR-0055 §6).
//!
//! A forced delete removes the pod object without waiting for its process: the
//! orchestrator recreates the ordinal at once, and two live incarnations of
//! one ordinal run side by side. The membership binds the ordinal to the
//! replacement's worker identity; the elder, which saw the ordinal bound to
//! its own identity, reads that as its supersession and stops authoring: no
//! SIP, no write, no CDR, and no drain.
//!
//! ```text
//!   established · b1#g1 serves · b2 synchronized
//!         orchestrator force-deletes b1: b1#g2 starts, the ordinal is
//!         re-bound to it — b1#g1's process still runs, holding the call
//!         b1#g1 reads its ordinal bound to b1#g2: superseded, silent
//!         b1#g2 reclaims the call from b2 and is judged Alive
//!   alice ──BYE──▶ proxy ──▶ b1#g2 ──▶ 200        (the call ends on the replacement)
//!         one keepalive interval on: b1#g1 has sent nothing and written no CDR
//!         b1#g1's process exits
//! ```

use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// Past one keepalive interval of the harness workers (300 s) and its
/// timeout (45 s): an elder still authoring would have probed the call's
/// dialogs and ended it with a CDR of its own by then.
const KEEPALIVE_AND_TIMEOUT: Duration = Duration::from_secs(360);

#[tokio::test(start_paused = true)]
async fn a_force_deleted_worker_stops_authoring_once_its_ordinal_is_rebound() {
    let mut fh = FailoverHarness::new("superseded-incarnation", &["b1", "b2"]);

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

    // ── an established, replicated call ──────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
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

    // ── the forced delete: a replacement, the ordinal re-bound to it ─────────
    let replacement = fh.force_delete(&pri_ord).await;
    let rebound_at = fh.sip_entries().len();
    assert_eq!(replacement.gen(), elder.gen() + 1, "the replacement is the next incarnation");
    fh.advance(Duration::from_millis(200)).await;
    assert!(elder.is_superseded(), "the elder read its ordinal bound to another identity");
    assert!(!replacement.is_superseded(), "the replacement serves its own ordinal");

    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(1)).await;

    // ── the call ends on the replacement ────────────────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let nodes: [&ReplicatedB2buaSut; 2] = [&replacement, survivor];
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(&call_ref).await
                && !nodes[1].holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");

    // ── the elder authored nothing after the re-binding ─────────────────────
    fh.advance(KEEPALIVE_AND_TIMEOUT).await;
    let elder_sip = elder.sip_addr();
    let authored: Vec<String> = fh.sip_entries()[rebound_at..]
        .iter()
        .filter(|e| e.from == elder_sip)
        .map(|e| e.start_line())
        .collect();
    assert!(authored.is_empty(), "the superseded elder sent SIP: {authored:?}");
    assert_eq!(
        total_cdrs_for(&[&*elder, &replacement, survivor], &call_ref),
        1,
        "one CDR across both incarnations and the backup"
    );

    // ── the elder's process exits; the call is released everywhere ─────────
    fh.mark(&pri_ord, None, "crash", "the superseded incarnation exits");
    elder.crash();
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&nodes[..], &call_ref).await;

    drop((w_b1, w_b2, replacement, proxy));
}
