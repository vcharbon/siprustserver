//! **A non-INVITE request the backup's copy relays, which its primary never
//! reads.** The proxy misroutes the caller's INFO to the backup while the
//! primary lives (ADR-0014); the copy relays it and the callee holds its
//! answer. The copy's write is held from the primary while the primary
//! relays the callee's INFO on its own: each copy made a compared write the
//! other never read, so they diverged (ADR-0055 §7), and the copy that meets
//! the primary's write ends the call cleanly; the call settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::establish;
use crate::crossed_reinvite_round::{info_crossing_the_end, info_through_the_primary};
use crate::diverged_end::{accept_the_clean_end, answer_the_clean_end};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

#[tokio::test(start_paused = true)]
async fn a_relay_the_primary_never_read_and_the_primarys_own_end_the_call() {
    let mut est = establish("relay-the-primary-never-read").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    est.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    accept_the_clean_end(&mut est);
    info_crossing_the_end(&est.alice, &mut est.bob_dialog).await;
    est.fh.resume(&b_ord, &p_ord);
    answer_the_clean_end(&mut est).await;
}

/// The same, the callee holding the caller's INFO through the clean end. The
/// copy keeps its relay through the end and answers it as its own teardown
/// does once the callee's dialog ended under its BYE: `481` to the caller
/// (RFC 3261 §15.1.2), whatever the callee then answers.
#[tokio::test(start_paused = true)]
async fn a_relay_the_copy_holds_through_the_end_draws_its_final() {
    let mut est = establish("relay-held-through-the-end").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_bob = est.bob.receive("INFO").await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    accept_the_clean_end(&mut est);
    info_crossing_the_end(&est.alice, &mut est.bob_dialog).await;
    info.expect_tolerating(481, &["BYE"]).await;
    info_at_bob.respond(487, "Request Terminated").await;
    est.fh.resume(&b_ord, &p_ord);
    answer_the_clean_end(&mut est).await;
}

/// The primary relays the caller's UPDATE, which both nodes read, so the
/// copy's body holds the primary's relay. The copy's writes are held while it
/// relays the callee's INFO, and the callee's 200 to the UPDATE reaches the
/// primary, which closes the relay: each copy wrote what the other never
/// read, so the copy ends the call on the primary's write. A relay the copy
/// holds only as the primary's, its final gone to the primary, is no record
/// of the copy's to answer.
#[tokio::test(start_paused = true)]
async fn a_relay_the_primary_made_and_closed_is_no_record_of_the_copys() {
    let mut est = establish("relay-closed-at-the-primary").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    let mut update = est.dialog.request(InDialogMethod::Update, None).await;
    let mut update_at_bob = est.bob.receive("UPDATE").await;
    est.fh.advance(SETTLE).await;
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    accept_the_clean_end(&mut est);
    update_at_bob.respond(200, "OK").await;
    update.expect_tolerating(200, &["BYE"]).await;
    est.fh.resume(&b_ord, &p_ord);
    answer_the_clean_end(&mut est).await;
}
