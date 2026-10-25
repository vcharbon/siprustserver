//! **A quiet call of a primary that departs for good.** The departure sweep
//! leaves a quiet established call reactive (ADR-0014 §1, ADR-0055 §5): no
//! party speaks, so no node takes it over. When the backup's replica of it
//! outlives its TTL, the reap ends it there — off the wire, its CDR written,
//! the call's only one (ADR-0055 §5).
//!
//! ```text
//!   established · P serves · B synchronized
//!   P crashes and leaves the set  (the sweep leaves the quiet call)
//!   the replica's TTL passes      (B's reap ends it, with its CDR)
//!   alice ──BYE──▶ B: 481         bob ──BYE──▶ B: 481
//! ```

use std::time::Duration;

use failover_harness::{assert_call_fully_released, total_cdrs_for, WorkerHealth};

use crate::discharged_call_dropped_by_its_primary::{establish, Established};

#[tokio::test(start_paused = true)]
async fn a_quiet_call_of_a_departed_primary_is_reaped_with_its_cdr() {
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
    } = establish("ghost-of-a-departed-primary").await;
    {
        let primary = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
        fh.mark(&pri_ord, None, "crash", "the primary crashes and leaves the set");
        primary.crash();
        proxy.set_health(&pri_ord, WorkerHealth::Dead);
    }
    fh.depart(&pri_ord);
    fh.advance(Duration::from_secs(1)).await;
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the sweep leaves the quiet call");

    let reaped = fh
        .settle_lossy_cleanup(async || {
            backup.memory_clean() && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(reaped, "the reap ends the call past the replica's TTL");
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 1, "the reap writes the call's one CDR");

    // The parties' own hangups meet no dialog (RFC 3261 §12.2.2).
    let mut bye = dialog.bye().await;
    bye.expect(481).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[backup], &call_ref).await;
    drop(proxy);
}
