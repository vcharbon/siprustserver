//! **A worker whose process dies just after its endpoint reads not ready
//! pulls nothing.** The not-ready flip wakes its membership loop, and the
//! crash parks its pullers: a reconcile the flip already woke spawns no
//! puller once the stop began. Its peer reads no flow from it, so nothing
//! vouches for a copy no live process holds.
//!
//! ```text
//!   b1 not ready, then crashed · a call on b2 names b1
//!   b2 reads no live flow from b1 · the caller hangs up · one CDR
//! ```
//!
//! Beyond the failover sweep: a crash right after a readiness flip.

use std::time::Duration;

use failover_harness::{total_cdrs_for, FailoverHarness, Partition};

use crate::away_backup_support::{
    answered_on, bring_up, caller_hangs_up, restart_in_place, settle, ALICE, BOB,
};

#[tokio::test(start_paused = true)]
async fn a_worker_crashed_after_its_readiness_flip_pulls_nothing() {
    let name = "crash-after-a-readiness-flip";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut a, b) = bring_up(&mut fh).await;

    restart_in_place(&mut fh, &mut a);
    fh.advance(Duration::from_secs(1)).await;
    let mut call = answered_on(&fh, &alice, &bob, &proxy, &b).await;
    fh.advance(Duration::from_secs(6)).await;
    assert!(!b.flow_caught_up("b1", Partition::Bak), "no flow of the crashed worker reaches b2");

    caller_hangs_up(&mut call.dialog, &bob).await;
    settle(&fh, &[&a, &b], &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&[&a, &b], &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
