//! **A non-INVITE request the backup's copy relays, which its primary never
//! reads.** The proxy misroutes the caller's INFO to the backup while the
//! primary lives (ADR-0014); the copy relays it and the callee holds its
//! answer. The copy's write is held from the primary while the primary
//! relays the callee's INFO on its own, so the primary refuses the copy's
//! write once released (ADR-0031 D3). The callee's answer follows the copy's
//! Via back to the copy (RFC 3261 §18.2.2), so the copy, which holds the
//! relay alone, keeps serving the call through the primary's writes until it
//! has relayed that answer: the caller's INFO is answered 200, and the call
//! ends with a BYE that settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::establish;
use crate::crossed_reinvite_round::{caller_hangs_up, info_through_the_primary};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

#[tokio::test(start_paused = true)]
async fn a_relay_the_primary_never_read_is_answered_through_the_copy() {
    let mut est = establish("relay-the-primary-never-read").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_bob = est.bob.receive("INFO").await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.fh.resume(&b_ord, &p_ord);
    est.fh.advance(SETTLE).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(SETTLE).await;

    info_at_bob.respond(200, "OK").await;
    info.expect(200).await;
    caller_hangs_up(&mut est).await;
}
