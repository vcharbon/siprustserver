//! Goal-1 **split-brain cutover (no pod reboot)** suite.
//!
//! The existing scenario suite either heals a *short* partition (well under the
//! tombstone TTL, so no reap) or crashes+reboots a node (so it returns *cold*,
//! bypassing the warm-reconnect paths). This file fills the gap the review
//! flagged: a node whose view of a peer is severed for **longer than the
//! tombstone/backstop TTL and then restored WITHOUT rebooting** — it keeps its
//! store and its retained watermark. That single shape exercises the cluster of
//! fixes:
//!
//! - a warm puller that fell off the compacted tail is told to re-bootstrap
//!   (`ResetToBootstrap`) instead of silently diverging;
//! - the supervisor pulls its retained watermark back down on that reset, so a
//!   respawn does not resume from the now-invalid high W;
//! - a missed-delete replica self-evicts via the body-TTL backstop;
//! - a crashed node spawns no pullers on later membership deltas.
//!
//! We drive the cutover via topology **remove → re-add** rather than a network
//! `partition`: the simulated partition fault keys on exact addr pairs, but the
//! supervisor's pullers connect from synthesised ephemeral local addresses, so a
//! partition does not actually sever an established puller (see s6_tests notes).
//! A topology remove parks the puller with its watermark retained — exactly the
//! "severed, store intact, no reboot" precondition — and a re-add reconnects it
//! warm. Fake-clock discipline per CLAUDE.md: advance to the deadline, assert.

use std::time::Duration;

use ha_harness::{backup_is, cref, HaCluster, PartitionRole, Peer, Watermark};

const BAK: PartitionRole = PartitionRole::Backup;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// ---------------------------------------------------------------------------
// 1. Long split-brain with a delete, restored WITHOUT reboot.
//
//    A backs a call up onto B. B's view of A is severed (puller parked, store +
//    watermark retained) for longer than the tombstone TTL, during which A
//    deletes the call and reaps — so the tombstone is gone and A's retention
//    floor for B rises above B's retained watermark. On re-add B reconnects WARM;
//    its `since` is below A's reaped tail, so A sends `ResetToBootstrap` instead
//    of silently leaving B with a ghost. The missed-delete replica self-evicts
//    via the backstop, and a fresh post-cutover write converges (B not stuck).
//
//    Without the fixes B keeps the deleted call forever — `ttl<=0` never evicts
//    AND no reset is ever sent — so `B.get(gone)` would still return the body.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn long_split_brain_delete_restored_without_reboot() {
    // Short replica backstop (8s) so a missed-delete replica self-evicts within
    // the test budget; the tombstone TTL stays at the 30s default. (A deleted
    // call is never re-flushed, so the backstop is the only thing that can drop
    // it on a backup that missed the delete.)
    let mut cl =
        HaCluster::with_replica_backstop_ms(&["A", "B"], sip_clock::Clock::test_at(0), 8_000).await;
    cl.advance(ms(100)).await;

    let gone = cref("A", "gone");
    cl.put("A", &gone, b"g1".to_vec(), 1, &backup_is("B")).await;
    cl.advance(ms(200)).await;
    assert_eq!(cl.node("B").get(BAK, "A", &gone).await.as_deref(), Some(&b"g1"[..]));
    let w_before = cl.node("B").watermark("A");
    assert_eq!(w_before, Watermark::new(1, 1), "B tailed the call");

    // --- sever B's view of A (park puller, retain store + W); delete on A ---
    cl.node("B").remove_peer("A");
    cl.advance(ms(100)).await;
    cl.delete("A", &gone, &backup_is("B")).await;

    // Hold the cutover past the tombstone TTL (30s), then reap on A: the `gone`
    // tombstone is evicted and A's retention floor for B rises past B's W.
    cl.advance(secs(40)).await;
    cl.node("A").reap(cl.now_ms()).await;
    assert_eq!(
        cl.node("B").watermark("A"),
        w_before,
        "B's watermark is retained across the cutover (no reboot)"
    );

    // --- restore: re-add A → B reconnects WARM → ResetToBootstrap → re-bootstrap ---
    cl.node("B").add_peer(Peer::new("A", "A"));
    cl.advance(secs(5)).await;

    // The deleted call's stale replica is gone: B missed the (reaped) delete, so
    // the only thing that can drop it is the backstop TTL — and it did.
    assert!(
        cl.node("B").get(BAK, "A", &gone).await.is_none(),
        "missed-delete replica self-evicted (not a permanent ghost)"
    );
    // B is not stuck below the reaped tail: a brand-new post-cutover write lands.
    let fresh = cref("A", "fresh");
    cl.put("A", &fresh, b"f1".to_vec(), 1, &backup_is("B")).await;
    cl.advance(secs(1)).await;
    assert_eq!(
        cl.node("B").get(BAK, "A", &fresh).await.as_deref(),
        Some(&b"f1"[..]),
        "post-cutover write converges — the stream was re-established"
    );
    assert!(cl.node("B").is_current("A"), "B is current again after re-bootstrap");
    // The watermark re-advanced (reset to (0,0) on ResetToBootstrap, then climbed
    // back via the re-bootstrap + tail — proof it did not stay stuck at w_before).
    assert!(
        cl.node("B").watermark("A") > w_before,
        "watermark un-stuck: reset then re-advanced past the old floor"
    );
}

// ---------------------------------------------------------------------------
// 2. A crashed node spawns NO pullers on later membership deltas (the reconcile
//    loop is aborted by crash → shutdown, not leaked → no double-replication).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn crashed_node_spawns_no_pullers_on_membership_delta() {
    let mut cl = HaCluster::new(&["A", "B"]).await;
    cl.advance(ms(100)).await;
    assert!(cl.node("A").is_running("B"), "A pulls B before the crash");

    // Crash A: shutdown() aborts the reconcile loop and parks the puller.
    cl.crash("A");
    assert!(!cl.node("A").is_running("B"), "crash parks A's pullers");

    // Drive a membership delta on the crashed node. A leaked reconcile loop would
    // react and spawn_puller(B) against the dead store; the aborted loop does not.
    cl.node("A").remove_peer("B");
    cl.node("A").add_peer(Peer::new("B", "B"));
    cl.advance(secs(1)).await;
    assert!(
        !cl.node("A").is_running("B"),
        "no puller respawns on a crashed node — the reconcile loop was aborted, not leaked"
    );
}
