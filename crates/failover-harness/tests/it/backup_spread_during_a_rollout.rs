//! **While one of three workers is away, the calls born on a second all name
//! the third as backup; past its replica ceiling the third refuses, and the
//! second's drain ends the refused calls off the wire at its grace**
//! (ADR-0037 item 4, ADR-0055 §3). A rollout concentrates backups: with b1
//! away, every call on b2 names b3. b3 holds three replicas at most; the
//! calls past them run on b2 alone. When the rollout reaches b2, its drain
//! exits on b3 holding the backed calls, and at its grace writes the CDR of
//! each refused call and releases it, sending nothing.
//!
//! ```text
//!   b1 drains, exits · five calls on b2, each w_bak=b3 · b3 stores three,
//!   refuses two · b1 returns · b2 drains ⇒ grace_peers_behind, the two
//!   refused ended with their CDRs, no SIP · b2 exits and returns ·
//!   backed calls: BYEs ─▶ 200 · refused calls: both BYEs ─▶ 481
//! ```
//!
//! Every call writes one CDR.
//!
//! Beyond the failover sweep: the backup ceiling under a rollout's backup
//! concentration.

use b2bua::capacity::{BackupBound, BackupCeilings};
use failover_harness::{total_cdrs_for, DrainExit, FailoverHarness};

use crate::away_backup_support::{
    answered_with, caller_hangs_up, come_back, drain, exits, settle, Answered, ALICE, BOB,
};
use crate::three_worker_support::{bring_up_three, call_id_placed_on, drain_clean};

/// The backup replicas each worker holds at most.
const CEILING: u64 = 3;

#[tokio::test(start_paused = true)]
async fn calls_past_the_concentrated_backups_ceiling_are_ended_off_the_wire_at_the_drain() {
    let name = "backup-spread-ceiling";
    let mut fh =
        FailoverHarness::new(name, &["b1", "b2", "b3"]).with_worker_replica(b2bua::ReplicaConfig {
            backup: BackupCeilings { calls: Some(CEILING), rss_bytes: None },
            ..Default::default()
        });
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut b1, mut b2, b3) = bring_up_three(&mut fh).await;

    // ── b1 restarts; every call born on b2 names b3 ─────────────────────────
    drain_clean(&mut fh, &b1).await;
    exits(&mut fh, &mut b1).await;
    let mut calls: Vec<Answered> = Vec::new();
    for i in 0..5 {
        let id = call_id_placed_on("b2", &["b2", "b3"], &format!("spread-{i}"));
        let call = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&id)).await;
        assert_eq!(call.cookie, ("b2".to_string(), "b3".to_string()), "each call names b3");
        calls.push(call);
    }
    let held = b3.scan_backed_up("b2");
    assert_eq!(held.len() as u64, CEILING, "b3 holds replicas up to its ceiling: {held:?}");
    assert_eq!(b3.capacity().backup_shed_total(BackupBound::Calls), 2, "b3 refused two");
    let (backed, refused): (Vec<Answered>, Vec<Answered>) =
        calls.into_iter().partition(|c| held.contains(&c.call_ref));
    come_back(&mut fh, &mut b1, &proxy).await;

    // ── b2 drains: b3 holds the backed calls, the refused end at the grace ──
    let t0 = fh.now_ms();
    let out = drain(&mut fh, &b2).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "no peer holds the refused: {out:?}");
    assert_eq!(out.residual, 5, "every live call the grace found, the backed ones included");
    for call in &backed {
        let cdrs = b2.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count();
        assert_eq!(cdrs, 0, "{} is left to b3", call.call_ref);
    }
    for call in &refused {
        let cdrs = b2.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count();
        assert_eq!(cdrs, 1, "b2 wrote {}'s CDR at the grace", call.call_ref);
        assert!(!b2.serves(&call.call_ref), "{} is released", call.call_ref);
    }
    let sent_by_b2 = fh
        .sip_entries()
        .iter()
        .filter(|e| e.from == b2.sip_addr() && e.sent_ms >= t0 as u64)
        .count();
    assert_eq!(sent_by_b2, 0, "the drain ends the refused calls off the wire");
    exits(&mut fh, &mut b2).await;
    come_back(&mut fh, &mut b2, &proxy).await;

    // ── the backed calls end normally, the refused ones draw 481 ────────────
    let mut backed = backed;
    for call in &mut backed {
        caller_hangs_up(&mut call.dialog, &bob).await;
    }
    let mut refused = refused;
    for call in &mut refused {
        let mut bye = call.dialog.bye().await;
        bye.expect(481).await;
        let mut bye = call.bob_dialog.bye().await;
        bye.expect(481).await;
    }
    let refs: Vec<&str> = backed.iter().chain(&refused).map(|c| c.call_ref.as_str()).collect();
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&nodes, r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
