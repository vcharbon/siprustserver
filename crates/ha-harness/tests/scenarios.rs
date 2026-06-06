//! Goal-1 scenario suite for the pure-HA-framework harness (plan "Test
//! architecture → Goal 1"). Every test runs under
//! `#[tokio::test(start_paused = true)]`; the protocol is driven BETWEEN
//! `cluster.advance(..)`s (advance to the deadline, then assert) per the
//! CLAUDE.md fake-clock hazards. The fabric coerces transit to `>= 1 ms`.
//!
//! These lift the b2bua repl-test wiring into the [`HaCluster`] packaging and
//! assert the goal-1 invariant: every reachable node's view of each call equals
//! the latest `call_gen`.

use std::time::Duration;

use ha_harness::{backup_is, cref, HaCluster, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// ---------------------------------------------------------------------------
// 1. write-converges: put on A → B has it; multi-put + update + delete converge.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn write_converges_put_update_delete() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    // Let B's puller open its subscription to A.
    cl.advance(ms(100)).await;

    let c1 = cref("A", "1");
    let c2 = cref("A", "2");
    cl.put("A", &c1, b"v1".to_vec(), 1, 0, &backup_is("B")).await;
    cl.put("A", &c2, b"w1".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(200)).await;

    assert_eq!(
        cl.node("B").get(BAK, "A", &c1).await.as_deref(),
        Some(&b"v1"[..]),
        "B has A's first call"
    );
    assert_eq!(
        cl.node("B").get(BAK, "A", &c2).await.as_deref(),
        Some(&b"w1"[..]),
        "B has A's second call"
    );

    // Update c1 (higher gen) + delete c2.
    cl.put("A", &c1, b"v2".to_vec(), 2, 0, &backup_is("B")).await;
    cl.delete("A", &c2, &backup_is("B")).await;
    cl.advance(ms(200)).await;

    assert_eq!(
        cl.node("B").get(BAK, "A", &c1).await.as_deref(),
        Some(&b"v2"[..]),
        "update converges to latest body"
    );
    assert!(
        cl.node("B").get(BAK, "A", &c2).await.is_none(),
        "delete removes c2 on B"
    );
    assert_eq!(cl.node("B").call_gen(BAK, "A", &c1), Some(2));
}

// ---------------------------------------------------------------------------
// 2. crash → reboot re-hydrates: crash a primary; reboot; reclaim via
//    bootstrap+tail.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn crash_then_reboot_rehydrates() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;

    let bodies = ["b0", "b1", "b2"];
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        cl.put("A", &c, body.as_bytes().to_vec(), 1, 0, &backup_is("B"))
            .await;
    }
    cl.advance(ms(200)).await;
    // B holds all three in bak:A.
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        assert_eq!(
            cl.node("B").get(BAK, "A", &c).await.as_deref(),
            Some(body.as_bytes()),
            "B backs up A's call {i}"
        );
    }

    // A crashes (memory wiped) then reboots empty under a higher gen.
    cl.crash("A");
    cl.advance(ms(100)).await;
    cl.reboot("A").await;
    assert_eq!(cl.node("A").gen(), 2, "reboot bumped incarnation gen");
    cl.advance(secs(1)).await;

    // Rebooted A reclaims all three as pri:A via bootstrap.
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        assert_eq!(
            cl.node("A").get(PRI, "A", &c).await.as_deref(),
            Some(body.as_bytes()),
            "rebooted A reclaims call {i} as pri:A"
        );
    }
    assert!(cl.node("A").is_bootstrapped("B"), "A bootstrap-complete from B");
}

// ---------------------------------------------------------------------------
// 3. partition → heal: mutations during partition converge after heal.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn partition_then_heal_converges() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;

    let c = cref("A", "1");
    cl.put("A", &c, b"before".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(200)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"before"[..])
    );

    // Partition A<->B, mutate during the cut.
    cl.partition("A", "B");
    cl.advance(ms(100)).await;
    cl.put("A", &c, b"during".to_vec(), 2, 0, &backup_is("B")).await;
    let c3 = cref("A", "3");
    cl.put("A", &c3, b"new-during".to_vec(), 1, 0, &backup_is("B"))
        .await;
    cl.advance(ms(100)).await;

    // Heal → converge.
    cl.heal("A", "B");
    cl.advance(secs(1)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"during"[..]),
        "update converges after heal"
    );
    assert_eq!(
        cl.node("B").get(BAK, "A", &c3).await.as_deref(),
        Some(&b"new-during"[..]),
        "new call created during partition converges after heal"
    );
}

// ---------------------------------------------------------------------------
// 4. takeover → reclaim: acting-backup mutates; original reboots & reclaims
//    highest call_gen.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn takeover_then_reclaim_highest_gen() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;

    let c = cref("A", "1");
    // A (primary) creates gen1, forward-replicates to B.
    cl.put("A", &c, b"g1".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(200)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"g1"[..])
    );

    // A crashes. B (acting-backup) takes over: mutate the call via the policy
    // (B does NOT own "A|.." → Reverse). Two takeovers, gen2 then gen3.
    cl.crash("A");
    cl.put("B", &c, b"g2".to_vec(), 2, 0, &backup_is("A")).await;
    cl.advance(ms(100)).await;
    cl.put("B", &c, b"g3".to_vec(), 3, 0, &backup_is("A")).await;
    cl.advance(ms(100)).await;

    // A reboots empty under a higher gen → reclaims the highest gen (3).
    cl.reboot("A").await;
    cl.advance(secs(1)).await;
    assert_eq!(
        cl.node("A").get(PRI, "A", &c).await.as_deref(),
        Some(&b"g3"[..]),
        "rebooted A reclaims the acting-backup's gen3 (LWW), not stale gen1"
    );
    assert_eq!(cl.node("A").call_gen(PRI, "A", &c), Some(3));
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"g3"[..]),
        "B holds gen3"
    );
}

// ---------------------------------------------------------------------------
// 5. dead-peer auto-clean: a peer disappears long enough that its changelog
//    cursor auto-cleans; re-bootstraps on return.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dead_peer_auto_clean_then_rebootstrap() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;

    let c = cref("A", "1");
    cl.put("A", &c, b"v1".to_vec(), 1, 0, &backup_is("B")).await;
    cl.advance(ms(200)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"v1"[..])
    );

    // B disappears (crash). While B is gone past the dead-peer TTL (300s), A's
    // changelog cursor for B auto-cleans on reap (the per-peer propagate ZSET is
    // dropped wholesale — its `last_active` is older than the dead-peer TTL).
    cl.crash("B");
    cl.advance(secs(320)).await;
    cl.node("A").reap(cl.now_ms()).await;

    // B reboots empty → re-bootstraps from A. After the auto-clean, the dropped
    // changelog cursor is re-seeded by A's next authoritative mutation (a fresh
    // put re-bumps B's propagate log) and B converges on the live set.
    cl.reboot("B").await;
    cl.advance(ms(200)).await;
    cl.put("A", &c, b"v2".to_vec(), 2, 0, &backup_is("B")).await;
    cl.advance(secs(1)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &c).await.as_deref(),
        Some(&b"v2"[..]),
        "B re-acquires the call after a dead-peer auto-clean + reboot + re-bump"
    );
    assert!(cl.node("B").is_bootstrapped("A"));
}

// ---------------------------------------------------------------------------
// 6. watermark survives disappear/reappear: topology remove → re-add; pulls only
//    deltas / converges.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn watermark_survives_remove_readd() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;

    for i in 0..3 {
        let c = cref("A", &i.to_string());
        cl.put("A", &c, format!("b{i}").into_bytes(), 1, 0, &backup_is("B"))
            .await;
    }
    cl.advance(ms(200)).await;
    // The replica data B holds for A rides the **Backup** flow (B backs up A's
    // Forward-flushed calls); its watermark tracks these deltas (the Reclaim
    // cursor is over A's empty `bak:{B}`).
    let w_after_3 = cl.node("B").flow_watermark("A", ha_harness::Partition::Bak);
    assert_eq!(w_after_3, ha_harness::Watermark::new(1, 3), "B tailed 3");

    // Remove A from B's membership → puller parks, W retained.
    cl.node("B").remove_peer("A");
    cl.advance(ms(100)).await;
    assert_eq!(
        cl.node("B").flow_watermark("A", ha_harness::Partition::Bak),
        w_after_3,
        "W retained across Park"
    );

    // While parked, A adds two more.
    for i in 3..5 {
        let c = cref("A", &i.to_string());
        cl.put("A", &c, format!("b{i}").into_bytes(), 1, 0, &backup_is("B"))
            .await;
    }

    // Re-add A → reconnect from retained W=(1,3), pull only the 2 deltas.
    cl.node("B").add_peer(ha_harness::Peer::new("A", "A"));
    cl.advance(secs(1)).await;
    for i in 0..5 {
        let c = cref("A", &i.to_string());
        assert_eq!(
            cl.node("B").get(BAK, "A", &c).await.as_deref(),
            Some(&format!("b{i}").into_bytes()[..]),
            "call {i} converged after re-add"
        );
    }
    assert_eq!(
        cl.node("B").flow_watermark("A", ha_harness::Partition::Bak),
        ha_harness::Watermark::new(1, 5),
        "W advanced to head via deltas"
    );
}
