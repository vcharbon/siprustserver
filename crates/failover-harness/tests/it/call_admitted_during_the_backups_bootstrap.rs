//! **A call admitted while its returning backup recovers reaches it**
//! (ADR-0055 §3). The backup was away past the backlog's high-water age with
//! calls named on it, so the change log toward it holds entries older than
//! 5 s. It returns and recovers its own partition first (its reclaim flow is
//! in flight), so its backup flow from the primary is not open yet when a new
//! call arrives: the backup is away, no slow consumer, and the primary admits
//! the call backed. The backup's own bootstrap of the primary's calls carries
//! it once its backup flow opens. The primary's drain then exits on the
//! backup holding every call.
//!
//! ```text
//!   a away 6 s          five calls on b, each w_bak=a
//!   a returns           b ─▶ a delayed 500 ms: its recovery is in flight
//!   alice ─INVITE─▶ b   w_bak=a, admitted backed while a is not ready
//!   a ready             it holds the six calls
//!   b drains ⇒ caught_up, exits; each caller's BYE ─▶ a ─▶ 200; one CDR each
//!
//!   marks 3/1           five calls on b while a is away: over the high water
//!   a's own bootstrap of b's calls in flight: a new call is admitted backed,
//!   the bootstrap or the live stream carries it; b drains ⇒ caught_up, the
//!   call survives with no CDR from b
//! ```
//!
//! Beyond the failover sweep: a new call against a backup still recovering.

use std::time::Duration;

use b2bua::repl::BacklogMarks;
use failover_harness::{total_cdrs_for, FailoverHarness, Partition, WorkerHealth};

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, drain_caught_up, exits, restart_in_place, settle,
    Answered, ALICE, BOB,
};

/// How late the primary's frames reach the returning backup: the bootstrap
/// stays in flight that long, well under the puller's idle bound (2 s).
const BOOTSTRAP_DELAY_MS: u64 = 500;

#[tokio::test(start_paused = true)]
async fn a_call_admitted_during_the_backups_bootstrap_reaches_it() {
    let name = "call-during-the-backups-bootstrap";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── a away past the backlog's high-water age, five calls named on it ────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_millis(500)).await;
    let mut calls: Vec<Answered> = Vec::new();
    for _ in 0..5 {
        let call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
        assert_eq!(
            call.cookie,
            ("b2".to_string(), "b1".to_string()),
            "the proxy names the restarting worker"
        );
        calls.push(call);
    }
    fh.advance(Duration::from_secs(6)).await;

    // ── a returns; its bootstrap is in flight when a new call arrives ───────
    fh.delay("b2", "b1", BOOTSTRAP_DELAY_MS);
    fh.mark("b1", None, "reboot", "a fresh incarnation at a new address");
    let addr = a.reboot().await;
    fh.note_worker_rebound("b1", addr);
    fh.advance(Duration::from_millis(100)).await;
    assert!(!a.is_ready(), "the bootstrap is still in flight");
    let late = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(late.cookie, ("b2".to_string(), "b1".to_string()), "the late call names a too");
    calls.push(late);
    assert_eq!(
        b.metrics().repl_new_calls_unbacked_total(),
        0,
        "the bootstrapping backup's flow left no call unbacked"
    );

    // ── a ready: it holds every call, the late one included ──────────────────
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || a.is_ready())
        .await;
    assert!(ready, "a finished its bootstrap");
    fh.heal("b2", "b1");
    fh.readmit("b1", addr);
    proxy.set_health("b1", WorkerHealth::Alive);
    let holds_all = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            let held = a.scan_backed_up("b2");
            calls.iter().all(|c| held.contains(&c.call_ref))
        })
        .await;
    assert!(holds_all, "a holds every call: {:?}", a.scan_backed_up("b2"));

    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;
    for call in &mut calls {
        caller_hangs_up(&mut call.dialog, &bob).await;
    }
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    settle(&fh, &[&a, &b], &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&a, &b], r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// The backlog toward the returning backup is over the high-water mark when a
/// call arrives while the backup's own bootstrap of the primary's calls is in
/// flight: a bootstrapping backup is no slow consumer, so the call is admitted
/// backed, and survives the primary's drain through the backup.
#[tokio::test(start_paused = true)]
async fn a_call_admitted_while_the_backup_bootstraps_a_backlog_over_high_water_survives() {
    let name = "call-during-the-backups-backup-bootstrap";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;
    b.set_backlog_marks(BacklogMarks {
        high_entries: 3,
        low_entries: 1,
        high_age: Duration::from_secs(600),
        low_age: Duration::from_secs(300),
    });

    // ── a away; five calls named on it, over the high-water mark ────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_millis(500)).await;
    let mut calls: Vec<Answered> = Vec::new();
    for _ in 0..5 {
        calls.push(answered_on(&fh, &alice, &bob, &proxy, &b).await);
    }

    // ── a returns; its backup flow from b opens and bootstraps ──────────────
    fh.delay("b2", "b1", BOOTSTRAP_DELAY_MS);
    fh.mark("b1", None, "reboot", "a fresh incarnation at a new address");
    let addr = a.reboot().await;
    fh.note_worker_rebound("b1", addr);
    let bootstrapping = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            b.flow_open("b1", Partition::Bak) && !b.flow_caught_up("b1", Partition::Bak)
        })
        .await;
    assert!(bootstrapping, "a's backup flow from b is open, its bootstrap in flight");
    let late = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(late.cookie, ("b2".to_string(), "b1".to_string()), "the late call names a");
    assert_eq!(
        b.metrics().repl_new_calls_unbacked_total(),
        0,
        "a bootstrapping backup is no slow consumer: the late call is backed"
    );
    calls.push(late);

    // ── a ready and published: it holds every call ──────────────────────────
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || a.is_ready())
        .await;
    assert!(ready, "a finished its recovery");
    fh.heal("b2", "b1");
    fh.readmit("b1", addr);
    proxy.set_health("b1", WorkerHealth::Alive);
    let holds_all = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            let held = a.scan_backed_up("b2");
            calls.iter().all(|c| held.contains(&c.call_ref))
        })
        .await;
    assert!(holds_all, "a holds every call: {:?}", a.scan_backed_up("b2"));

    // ── b drains on a holding them; no CDR from b ───────────────────────────
    drain_caught_up(&mut fh, &b).await;
    for call in &calls {
        assert_eq!(
            b.cdr_records().iter().filter(|r| r.call_ref == call.call_ref).count(),
            0,
            "no CDR from the draining node for {}",
            call.call_ref
        );
    }
    exits(&mut fh, &mut b).await;
    for call in &mut calls {
        caller_hangs_up(&mut call.dialog, &bob).await;
    }
    let refs: Vec<&str> = calls.iter().map(|c| c.call_ref.as_str()).collect();
    settle(&fh, &[&a, &b], &refs, &[&alice, &bob]).await;
    for r in refs {
        assert_eq!(total_cdrs_for(&[&a, &b], r), 1, "one CDR for {r}");
    }
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
