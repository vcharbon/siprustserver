//! **A withdrawn worker observes its own withdrawal and latches Draining without
//! SIGTERM (ADR-0031 D6).**
//!
//! The informer shows a worker every endpoint of its pool, its own included. When
//! the orchestrator withdraws the endpoint — proxy and peers drop it — the
//! worker's own membership drops it too, and the worker, having seen itself
//! routable before, latches `Draining` on its own: OPTIONS and `/ready` report
//! draining, `is_withdrawn` holds, the withdrawn-but-running gauge is raised.
//! Nothing stands down (D5): the process keeps serving the call it holds.
//!
//! ```text
//!   established · elder serves · survivor synchronized
//!         orchestrator withdraws the elder  (registry + peers + its own view)
//!         the elder latches Draining by itself — no SIGTERM was sent
//!         orchestrator starts and readmits a replacement, kills the elder
//!   alice ──BYE──▶ proxy ──▶ replacement ──▶ 200
//! ```

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

#[tokio::test(start_paused = true)]
async fn withdrawn_worker_latches_draining_without_sigterm() {
    let mut fh = FailoverHarness::new("withdrawn-worker-latches-draining", &["b1", "b2"]);

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
    for w in [&w_b1, &w_b2] {
        assert!(!w.is_withdrawn() && !w.is_draining(), "a published worker is not withdrawn");
        assert!(!w.metrics().withdrawn_running());
    }

    // ── an established, replicated call on whichever worker the cookie picked ──
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

    // ── the orchestrator withdraws the endpoint; no SIGTERM is ever sent ──────
    fh.withdraw(&pri_ord);
    fh.advance(Duration::from_secs(1)).await;
    assert!(proxy.health(&pri_ord).is_none(), "the withdrawn ordinal no longer resolves");
    assert!(elder.is_withdrawn(), "the elder observed its own withdrawal");
    assert!(elder.is_draining(), "and latched Draining on its own");
    assert!(!elder.is_ready(), "its readiness reports draining, not ready");
    assert!(elder.metrics().withdrawn_running(), "withdrawn-but-running is on the gauge");
    assert!(elder.serves(&call_ref), "the withdrawn worker keeps serving its call (D5)");
    assert!(
        survivor.is_ready() && !survivor.is_withdrawn() && !survivor.is_draining(),
        "the survivor saw a peer leave, not itself"
    );
    assert!(!survivor.metrics().withdrawn_running());

    // ── a replacement of the SAME ordinal, then the elder is killed ───────────
    let replacement = fh.spawn_replacement(&pri_ord).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(!replacement.is_withdrawn(), "unpublished is not withdrawn: it was never routable");
    fh.readmit(&pri_ord, replacement.sip_addr());
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    assert!(!replacement.is_withdrawn(), "a fresh incarnation starts unwithdrawn");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(1)).await;
    assert!(elder.is_withdrawn() && elder.is_draining(), "the readmission un-withdraws nothing");

    fh.mark(&pri_ord, None, "crash", "the replaced incarnation is killed");
    elder.crash();
    fh.advance(Duration::from_secs(1)).await;

    // ── the call ends on the replacement, cleanly ─────────────────────────────
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
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(&nodes[..], &call_ref).await;

    // ── what the ledger recorded: the elder called itself draining, unprompted ─
    let ledger = fh.view_ledger();
    let elder_view = ledger.beliefs(&format!("{pri_ord}#g1"), &pri_ord);
    assert_eq!(
        elder_view,
        vec![Belief::Running { gen: 1 }, Belief::Draining { gen: 1 }, Belief::Dead { gen: 1 }],
        "running, then draining on its own observation, then dead: {elder_view:?}",
    );
    let survivor_view = ledger.beliefs(&format!("{}#g1", survivor.ordinal()), survivor.ordinal());
    assert_eq!(
        survivor_view,
        vec![Belief::Running { gen: 1 }],
        "the survivor never drained: {survivor_view:?}",
    );

    drop((w_b1, w_b2, replacement, proxy));
}
