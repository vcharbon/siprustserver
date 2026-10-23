//! **A call's end deferred to a primary that then departs for good.** A
//! backup's takeover copy that ends a call while its primary is still a member
//! of the set defers the discharge to that primary (ADR-0020 X3): it keeps the
//! ended Element and writes no CDR. When the primary then leaves the set
//! without having reconciled it, the departure sweep discharges the ended
//! Element — CDR, limiter release, delete — once (ADR-0047 D8).
//!
//! ```text
//!   established · P serves · B synchronized
//!   P crashes in place (its endpoint stays in the set)
//!   alice ──BYE──▶ proxy ──▶ B   (takeover copy: ends the call, defers)
//!   P's endpoint leaves the set  (B's departure sweep discharges the call)
//! ```

use std::time::Duration;

use failover_harness::{assert_call_fully_released, total_cdrs_for, WorkerHealth};

use crate::discharged_call_dropped_by_its_primary::{establish, Established};

#[tokio::test(start_paused = true)]
async fn an_end_deferred_to_a_primary_that_departs_is_discharged_by_the_sweep() {
    let Established {
        mut fh,
        alice,
        bob,
        proxy,
        mut w_b1,
        mut w_b2,
        pri_ord,
        call_ref,
        mut dialog,
    } = establish("deferred-end-of-a-departed-primary").await;
    {
        let primary = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
        fh.mark(&pri_ord, None, "crash", "the primary crashes in place");
        primary.crash();
        proxy.set_health(&pri_ord, WorkerHealth::Dead);
    }
    fh.advance(Duration::from_millis(300)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the backup defers to a member primary");
    assert!(backup.holds_any_trace(&call_ref).await, "and keeps the ended call for it");

    fh.depart(&pri_ord);
    let released = fh
        .settle_terminal(async || backup.memory_clean() && !backup.holds_any_trace(&call_ref).await)
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 1, "the sweep writes the call's one CDR");
    assert_eq!(backup.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert!(released, "the ended call is deleted");
    assert!(
        fh.now_ms() < b2bua::ReplicaConfig::default().reboot_budget_sec * 1000,
        "discharged by the sweep, not reaped at the replica TTL"
    );
    assert_call_fully_released(&[backup], &call_ref).await;
    drop(proxy);
}

/// The residual of the sweep's discharge (ADR-0047): a primary that
/// reconciled a deferred end — its CDR written, its delete sent — and left the
/// set before the backup applied that delete. The departure parks the
/// backup's flows, so the delete never lands, and the sweep discharges the
/// ended Element again: two CDRs. The window runs from the copy's flush to the
/// park; here the primary's writes to the backup are held throughout.
#[tokio::test(start_paused = true)]
async fn an_end_reconciled_by_a_primary_that_departs_before_its_delete_lands_is_discharged_twice() {
    let Established { mut fh, alice, bob, proxy, w_b1, w_b2, pri_ord, call_ref, mut dialog } =
        establish("deferred-end-reconciled-then-departed").await;
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    fh.stall(&pri_ord, bak_ord);
    // The proxy reads the running primary dead: the BYE reaches the backup.
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let reconciled = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            total_cdrs_for(&[primary], &call_ref) == 1
        })
        .await;
    assert!(reconciled, "the primary reconciled the backup's deferred end");
    assert!(backup.holds_any_trace(&call_ref).await, "its delete never reached the backup");

    fh.depart(&pri_ord);
    let released = fh
        .settle_terminal(async || {
            primary.memory_clean()
                && backup.memory_clean()
                && !primary.holds_any_trace(&call_ref).await
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert!(released, "nothing of the call is left");
    assert_eq!(total_cdrs_for(&[primary, backup], &call_ref), 2, "the residual: two CDRs");
    drop(proxy);
}
