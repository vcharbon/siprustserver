//! **A call admitted while its backup restarts names that backup, and survives
//! a later graceful drain of its primary** (ADR-0055 §3, §8). The other
//! worker is away when the call arrives: its container restarts behind a
//! not-ready endpoint, its pod is re-created at a new address, or it is back
//! but not yet read alive. The proxy names it as the call's backup all the
//! same, the primary admits the call backed, and the returning worker's
//! bootstrap carries the call. The primary's drain then exits on that backup
//! holding the call, and the call ends through the backup.
//!
//! ```text
//!   a away (not ready | departed | back, Unknown)
//!   alice ─INVITE─▶ proxy ─▶ b (only worker taking calls)   cookie w_pri=b w_bak=a
//!   a returns, bootstraps: it holds the call
//!   b drains ⇒ caught_up, exits
//!   alice (or bob) ─BYE─▶ proxy ─▶ a ─▶ 200; one CDR
//! ```
//!
//! Beyond the failover sweep: the backup a call admitted during its backup's
//! restart names, and a drain of its primary after the backup returned.

use failover_harness::{total_cdrs_for, FailoverHarness, ReplicatedB2buaSut};

use crate::away_backup_support::{
    answered_on, bring_up, callee_hangs_up, caller_hangs_up, come_back, come_back_unknown,
    drain_caught_up, exits, pod_deleted, restart_in_place, settle, ALICE, BOB,
};

/// Which party hangs up once the primary has gone.
#[derive(Clone, Copy)]
enum HangsUp {
    Caller,
    Callee,
}

/// The backup returned with the call: it holds the call's replica.
fn holds_the_replica(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) {
    assert!(
        backup.scan_backed_up(primary).iter().any(|r| r == call_ref),
        "the returned backup {} holds {call_ref} from its bootstrap",
        backup.ordinal()
    );
}

/// The container of `a` restarts in place; a call admitted on `b` meanwhile
/// names `a`, `a` returns and bootstraps it, `b` drains and exits, and
/// `who` hangs up through `a`.
async fn restart_in_place_then_drain(name: &str, who: HangsUp) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    // ── a's container restarts; a call arrives meanwhile ────────────────────
    restart_in_place(&mut fh, &mut a);
    fh.advance(std::time::Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), "b1".to_string()),
        "the proxy names the restarting worker as the call's backup"
    );

    // ── a returns and bootstraps the call ───────────────────────────────────
    come_back(&mut fh, &mut a, &proxy).await;
    holds_the_replica(&a, "b2", &call.call_ref);

    // ── b drains on a holding the call, and exits ───────────────────────────
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;

    // ── the call ends through a ─────────────────────────────────────────────
    match who {
        HangsUp::Caller => caller_hangs_up(&mut call.dialog, &bob).await,
        HangsUp::Callee => callee_hangs_up(&mut call.bob_dialog, &alice).await,
    }
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_call_admitted_during_its_backups_restart_survives_the_primarys_drain() {
    restart_in_place_then_drain("backup-restart-caller-hangs-up", HangsUp::Caller).await;
}

/// The callee's BYE follows the b-leg cookie, which names the same backup.
#[tokio::test(start_paused = true)]
async fn the_callees_bye_reaches_the_backup_named_during_its_restart() {
    restart_in_place_then_drain("backup-restart-callee-hangs-up", HangsUp::Callee).await;
}

/// The pod of `a` is re-created: its endpoint left the slice, and only the
/// proxy's tombstone of its address still names it when the call arrives. It
/// returns at a new address.
#[tokio::test(start_paused = true)]
async fn a_call_admitted_while_its_backups_pod_is_recreated_survives_the_primarys_drain() {
    let name = "backup-pod-recreated";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    pod_deleted(&mut fh, &mut a);
    fh.advance(std::time::Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), "b1".to_string()),
        "the proxy names the departed worker its tombstone still knows"
    );

    come_back(&mut fh, &mut a, &proxy).await;
    holds_the_replica(&a, "b2", &call.call_ref);
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;

    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// `a` is back and ready but the proxy has not read it alive yet: the call
/// admitted then names it, and reaches it on the live stream.
#[tokio::test(start_paused = true)]
async fn a_call_admitted_before_its_returned_backup_reads_alive_names_it() {
    let name = "backup-back-unknown";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, mut b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    fh.advance(std::time::Duration::from_secs(1)).await;
    come_back_unknown(&mut fh, &mut a).await;
    assert_eq!(
        proxy.health("b1"),
        Some(failover_harness::WorkerHealth::Unknown),
        "the returned worker is not read alive yet"
    );
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), "b1".to_string()),
        "the proxy names the returned worker it does not read alive yet"
    );
    holds_the_replica(&a, "b2", &call.call_ref);

    proxy.set_health("b1", failover_harness::WorkerHealth::Alive);
    fh.advance(std::time::Duration::from_millis(200)).await;
    drain_caught_up(&mut fh, &b).await;
    exits(&mut fh, &mut b).await;

    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
