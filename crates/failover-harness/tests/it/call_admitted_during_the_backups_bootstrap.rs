//! **A call admitted while its returning backup bootstraps reaches it**
//! (ADR-0055 §3). The backup was away past the backlog's high-water age with
//! calls named on it, so the change log toward it holds entries older than
//! 5 s. It returns and its bootstrap is in flight (its flow is open, it has
//! claimed no position yet) when a new call arrives: that flow is no slow
//! consumer, so the primary admits the call backed, past the snapshot the
//! bootstrap scanned, and the live stream carries it. The primary's drain
//! then exits on the backup holding every call.
//!
//! ```text
//!   a away 6 s          five calls on b, each w_bak=a
//!   a returns           b ─▶ a delayed 500 ms: the bootstrap is in flight
//!   alice ─INVITE─▶ b   w_bak=a, admitted backed while a is not ready
//!   a ready             it holds the six calls
//!   b drains ⇒ caught_up, exits; each caller's BYE ─▶ a ─▶ 200; one CDR each
//! ```
//!
//! Beyond the failover sweep: a new call against a backup in its bootstrap.

use std::time::Duration;

use failover_harness::{total_cdrs_for, FailoverHarness, WorkerHealth};

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
