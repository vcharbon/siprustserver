//! **An endpoint withdrawn while its process keeps running, then replaced.**
//!
//! The orchestrator can take a worker out of the cluster's view without ending
//! its process: the front proxy's registry drops the ordinal and every peer's
//! membership drops it, while the process stays bound, keeps its live calls and
//! keeps answering whatever still reaches it. A replacement of the SAME ordinal
//! can then come up beside it — two incarnations of one ordinal at once, the
//! elder invisible to everyone but itself.
//!
//! ```text
//!   established · b1#g1 serves · b2 synchronized
//!         orchestrator withdraws b1  (registry + peers drop it; PROCESS RUNS)
//!         orchestrator starts b1#g2  (gen 2, new pod IP, new repl socket)
//!         orchestrator readmits b1   (published at b1#g2's address, Unknown)
//!         b1#g2 reclaims the call from b2 and is judged Alive
//!         the elder incarnation is killed
//!   alice ──BYE──▶ proxy ──▶ b1#g2 ──▶ 200        (the call ends on the replacement)
//! ```
//!
//! What this pins is the HARNESS and its report, not a SUT behaviour: the
//! membership primitives exist and act on the real components, an ordinal with
//! two live incarnations renders as two sub-lanes, and the views ledger records
//! the disagreement at its heart — the proxy calls `b1` absent while `b1#g1`'s
//! process is up and serving. The call is still driven to a proper, fully
//! released end with exactly one CDR.

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
async fn withdrawn_endpoint_replaced_while_its_process_still_runs() {
    let mut fh = FailoverHarness::new("withdrawn-endpoint-replacement", &["b1", "b2"]);

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

    // ── the orchestrator withdraws the endpoint; the process keeps running ────
    fh.withdraw(&pri_ord);
    fh.advance(Duration::from_secs(1)).await;
    assert!(proxy.health(&pri_ord).is_none(), "the withdrawn ordinal no longer resolves");
    assert!(elder.serves(&call_ref), "the withdrawn worker's process still serves its call");

    // ── a replacement of the SAME ordinal, alongside the running incarnation ──
    let replacement = fh.spawn_replacement(&pri_ord).await;
    assert_eq!(replacement.gen(), elder.gen() + 1, "the replacement is the next incarnation");
    assert_ne!(replacement.sip_addr(), elder.sip_addr(), "a replacement binds a new pod IP");
    fh.readmit(&pri_ord, replacement.sip_addr());
    assert_eq!(
        proxy.health(&pri_ord),
        Some(WorkerHealth::Unknown),
        "a re-admitted endpoint is Unknown until something judges it",
    );

    // The replacement re-hydrates its own partition from the survivor.
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(1)).await;

    // ── the elder incarnation is finally killed ───────────────────────────────
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

    // ── what the ledger recorded ─────────────────────────────────────────────
    let ledger = fh.view_ledger();
    let proxy_view = ledger.beliefs("proxy", &pri_ord);
    let absent_at = proxy_view
        .iter()
        .position(|b| *b == Belief::Absent)
        .expect("the proxy dropped the withdrawn ordinal");
    assert_eq!(
        proxy_view[0],
        Belief::Registered(WorkerHealth::Alive),
        "the proxy started out holding the worker alive: {proxy_view:?}",
    );
    assert!(
        proxy_view[absent_at..].contains(&Belief::Registered(WorkerHealth::Unknown)),
        "the re-admitted endpoint came back Unknown: {proxy_view:?}",
    );
    assert!(
        proxy_view[absent_at..].contains(&Belief::Registered(WorkerHealth::Alive)),
        "and was judged Alive again: {proxy_view:?}",
    );

    let survivor_view = ledger.beliefs(&format!("{bak_ord}#g1"), &pri_ord);
    assert!(
        survivor_view.contains(&Belief::PeerParked),
        "the survivor parked the departed peer: {survivor_view:?}",
    );
    assert_eq!(
        survivor_view.last(),
        Some(&Belief::PeerActive),
        "and re-activated it on readmission: {survivor_view:?}",
    );

    assert_eq!(
        ledger.beliefs("orchestrator", &pri_ord),
        vec![Belief::Withdrawn, Belief::Running { gen: 2 }, Belief::Admitted],
        "the orchestrator's three acts: withdraw, replace, readmit",
    );

    // The elder's process outlived its endpoint: while the proxy called the
    // ordinal absent, the incarnation still called itself running.
    let elder_view = ledger.beliefs(&format!("{pri_ord}#g1"), &pri_ord);
    assert_eq!(elder_view.first(), Some(&Belief::Running { gen: 1 }));
    assert_eq!(elder_view.last(), Some(&Belief::Dead { gen: 1 }), "{elder_view:?}");

    // ── the report carries the views table and the disagreement ──────────────
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports/withdrawn-endpoint-replacement");
    let written = fh
        .write_unified_report(
            &dir,
            "report",
            "A withdrawn endpoint whose process keeps running, then a replacement",
            true,
        )
        .expect("report written");
    let html = std::fs::read_to_string(&written[0]).expect("report html");
    assert!(html.contains("Views — what each observer believed"), "the views table is rendered");
    assert!(html.contains("class=\"disputed"), "disagreeing cells are marked");
    assert!(!html.contains("Disagreements (0)"), "the run recorded a disagreement");
    // The ordinal ran two incarnations at once, so its column fanned out.
    assert!(html.contains(&format!("{pri_ord}#g1 (")), "the elder incarnation has its own lane");
    assert!(html.contains(&format!("{pri_ord}#g2 (")), "the replacement has its own lane");
    eprintln!("report: {}", written[0].display());

    drop((w_b1, w_b2, replacement, proxy));
}
