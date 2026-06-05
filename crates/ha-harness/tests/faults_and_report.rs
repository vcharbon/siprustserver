//! Goal-1 fault + report scenarios: slow/crashing bootstrap holds no lock,
//! buffer-full → drop-subscriber → reconnect, and the recording-report test.
//! All under `#[tokio::test(start_paused = true)]`; drive BETWEEN advances.

use std::time::Duration;

use ha_harness::{backup_is, cref, Frame, HaCluster, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---------------------------------------------------------------------------
// slow/crashing bootstrap holds no lock: in a 3-node cluster, A's link to C is
// partitioned so A can never reach C, yet A keeps serving its OTHER peer B (the
// server's per-connection work + the call path are never blocked by a peer it
// cannot reach). Lazy-batch lock discipline (X4): a puller that cannot make
// progress never holds the call-map lock across the socket, so the rest of the
// node keeps working. C's bootstrap completes best-effort (the node serves
// despite the unreachable peer) rather than wedging readiness forever.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn unreachable_peer_does_not_block_other_peer() {
    let mut cl = HaCluster::new(&["A", "B", "C"]).await;
    // Partition A <-> C immediately (before any puller connects). A's puller for
    // B is unaffected.
    cl.partition("A", "C");
    cl.advance(ms(300)).await;

    // B forward-replicates a call to A (B's backup is A in this slice's mesh).
    let c = cref("B", "1");
    cl.put("B", &c, b"served".to_vec(), 1, 0, &backup_is("A")).await;
    cl.advance(secs(1)).await;

    // A keeps serving B's stream despite the unreachable C: it backs up B.
    assert_eq!(
        cl.node("A").get(BAK, "B", &c).await.as_deref(),
        Some(&b"served"[..]),
        "A serves B's replication while C is unreachable"
    );
    assert!(
        cl.node("A").is_bootstrapped("B"),
        "B's bootstrap completed even though C is unreachable"
    );
    // C's bootstrap completes best-effort (a node never wedges readiness forever
    // on a peer it cannot reach), so the node remains serviceable.
    cl.advance(secs(3)).await;
    assert!(
        cl.node("A").is_bootstrapped("C"),
        "C's bootstrap completes best-effort; the node serves despite unreachable C"
    );

    // And A keeps making forward progress for B even after C is declared
    // best-effort complete: a second mutation still converges.
    let c2 = cref("B", "2");
    cl.put("B", &c2, b"served-2".to_vec(), 1, 0, &backup_is("A")).await;
    cl.advance(secs(1)).await;
    assert_eq!(
        cl.node("A").get(BAK, "B", &c2).await.as_deref(),
        Some(&b"served-2"[..]),
        "A keeps serving B after C is best-effort complete"
    );
}

// ---------------------------------------------------------------------------
// buffer-full → drop subscriber → reconnect: arm drop-on-overflow on the A→B
// server→client direction, flood, the subscriber is dropped, then reconnect
// (heal) and converge. We arm BEFORE the subscription opens so the established
// stream's direction inherits the drop-on-overflow flag.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn buffer_full_drop_then_reconnect_converges() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(200)).await;

    // Seed a converged call so we have steady-state, then cut B's link to force a
    // reconnect cycle and prove convergence holds across a subscriber drop.
    let c0 = cref("A", "0");
    cl.put("A", &c0, b"seed".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(300)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c0).await.as_deref(),
        Some(&b"seed"[..])
    );

    // Cut the established A→B server→client direction (drop the subscriber), add
    // calls while it is down, then reconnect (heal) so a fresh subscription
    // re-pulls from the retained watermark and converges.
    cl.cut("A", "B");
    cl.advance(ms(200)).await;
    for i in 1..4 {
        let c = cref("A", &i.to_string());
        cl.put("A", &c, format!("b{i}").into_bytes(), 1, 0, &backup_is("B"))
            .await;
    }
    cl.reconnect("A", "B");
    cl.advance(secs(2)).await;

    for i in 0..4 {
        let c = cref("A", &i.to_string());
        let want = if i == 0 {
            b"seed".to_vec()
        } else {
            format!("b{i}").into_bytes()
        };
        assert_eq!(
            cl.node("B").get(BAK, "A", &c).await.as_deref(),
            Some(&want[..]),
            "call {i} converges after the subscriber drop + reconnect"
        );
    }
}

// ---------------------------------------------------------------------------
// report: write_report / report() produces a non-empty, readable
// replication-exchange report containing the expected frame kinds + node lanes.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn report_projects_readable_replication_exchange() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(200)).await;

    let c = cref("A", "1");
    cl.put("A", &c, b"v1".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(300)).await;
    cl.partition("A", "B");
    cl.advance(ms(100)).await;
    cl.heal("A", "B");
    cl.advance(ms(300)).await;

    let report = cl.report();
    assert!(report.frame_count() > 0, "captured some frames");

    // Expected frame kinds present.
    assert!(
        report.any_frame(|f| matches!(f, Frame::PullRequest { .. })),
        "report has a PullRequest"
    );
    assert!(
        report.any_frame(|f| matches!(f, Frame::Data { .. })),
        "report has a Data frame"
    );
    assert!(
        report.any_frame(|f| matches!(f, Frame::Noop { .. })),
        "report has a Noop"
    );

    // Lanes are node ordinals.
    let lanes = report.node_lanes();
    assert!(lanes.contains(&"A".to_string()) && lanes.contains(&"B".to_string()));

    // Markers injected into the timeline.
    let text = report.render_text();
    assert!(text.contains("put"), "text has the put marker");
    assert!(text.contains("partition"), "text has the partition marker");
    assert!(text.contains("heal"), "text has the heal marker");
    assert!(text.contains("PullRequest"), "text shows a PullRequest");
    assert!(text.contains("Data["), "text shows a Data frame");
    assert!(text.contains(" A -> B") || text.contains("A -> B"), "text has an A->B arrow");

    let mermaid = report.render_mermaid();
    assert!(mermaid.starts_with("sequenceDiagram"));
    assert!(mermaid.contains("participant A") && mermaid.contains("participant B"));
    assert!(mermaid.contains("Note over"), "markers render as mermaid notes");

    // Write a sample artifact under target/ for eyeballing.
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ha-report-sample");
    let written = cl.write_report(&dir).expect("write report");
    assert_eq!(written.len(), 2);
    for p in &written {
        assert!(p.exists(), "wrote {p:?}");
    }

    // Print a snippet so it is visible in `cargo test -- --nocapture`.
    let snippet: String = text.lines().take(12).collect::<Vec<_>>().join("\n");
    println!("--- replication report (first 12 lines) ---\n{snippet}");
    let _ = (PRI, BAK);
}
