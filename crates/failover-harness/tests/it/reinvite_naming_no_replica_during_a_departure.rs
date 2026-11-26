//! **A re-INVITE that names no call here while its primary's departure is in
//! progress.** The call's replica never reached the backup when its primary
//! died and left the membership set; the caller's re-INVITE fails over to the
//! survivor, which finds no call and answers 481 (RFC 3261 §12.2.2). The
//! replica then lands, as a pull that lagged behind the death lands it, and a
//! copy of the re-INVITE that crossed the 481 arrives. Answered as a stateless
//! UAS answers (§8.2.7), the copy would be a first arrival, take the call over
//! from the landed replica and relay the round: the caller would hold a 481
//! and then a 2xx for one transaction (§17.1.1). While a peer has departed
//! the survivor answers through the INVITE's transaction, which absorbs the
//! copy (§17.2.1); the call is then ended by the caller's BYE, taken over from
//! the replica, with one CDR.

use std::time::Duration;

use failover_harness::{FailoverHarness, PartitionRole};
use sip_message::Method;

use crate::away_backup_support::answered_with;
use crate::departure_support::{call_id_placed_on, kill_announced};
use crate::inflight_resend::{
    bring_up, last_request_sent, reboot_and_reclaim, round_invites_sent_to, settle_to_one_cdr,
    REOFFER,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The time a copy and its answers take on the wire, far inside T1.
const SETTLE: Duration = Duration::from_millis(20);

#[tokio::test(start_paused = true)]
async fn a_reinvite_naming_no_replica_during_a_departure_absorbs_its_copy() {
    let mut fh = FailoverHarness::new("reinvite-no-replica-departure", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, w_b2) = bring_up(&mut fh).await;

    fh.stall("b1", "b2");
    let call_id = call_id_placed_on("b1", "no-replica");
    let mut call = answered_with(&fh, &alice, &bob, &proxy, &w_b1, Some(&call_id)).await;
    let call_ref = call.call_ref.clone();
    assert!(w_b2.scan_backed_up("b1").is_empty(), "the call's replica is held on the fabric");
    let primary = PartitionRole::Primary;
    let body = w_b1.get(primary, "b1", &call_ref).await.expect("the primary's record");
    let version = (
        w_b1.call_gen(primary, "b1", &call_ref).expect("its primary version"),
        w_b1.call_bgen(primary, "b1", &call_ref).expect("its backup version"),
    );
    let hydrated_before = kill_announced(&mut fh, &mut w_b1, &w_b2, "b1", &proxy).await;

    let mut reinvite = call.dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(481).await;
    let (copy, toward) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    w_b2.land_replica("b1", &call_ref, body, version).await;
    alice.try_send_datagram(&copy, toward).await.expect("the copy leaves");
    fh.advance(SETTLE).await;
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 0, "no node relays the round");
    assert_eq!(
        w_b2.metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "the copy takes nothing over: the INVITE's transaction absorbs it",
    );
    drop(reinvite);

    let mut bye = call.dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    fh.heal("b1", "b2");
    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, &mut w_b1, &w_b2, "b1", &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
