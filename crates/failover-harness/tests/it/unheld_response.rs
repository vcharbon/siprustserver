//! **A response for a call no node holds** is answered as an unknown dialog
//! would be (ADR-0055 §4): a non-2xx final to an INVITE is ACKed hop by hop
//! at the Request-URI the sender's via-extension states (RFC 3261 §17.1.1.3,
//! ADR-0055 §12), so the callee stops retransmitting it (§17.2.1).

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, OFFER};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The primary relays the caller's re-INVITE while its writes are held from
/// the backup; the callee holds its final. The proxy reads the primary dead:
/// the callee hangs up through the copy, which ends the call, and the primary
/// takes that ending: it answers the caller's re-INVITE 487 and CANCELs the
/// round toward the callee. The proxy still reads the primary dead, so the
/// callee's 487 reaches the backup, which no longer holds the call: it ACKs
/// the 487 from the via-extension, and the callee's INVITE transaction
/// completes.
#[tokio::test(start_paused = true)]
async fn a_487_failed_over_to_the_node_that_released_the_call_is_acked() {
    let mut est = establish("unheld-487-acked").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&p_ord, &b_ord);
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut bye = est.bob_dialog.bye().await;
    est.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    round.expect(487).await;
    est.bob.receive("CANCEL").await.respond(200, "OK").await;
    est.fh.advance(SETTLE).await;
    at_bob.respond(487, "Request Terminated").await;
    let ack = est.bob.receive("ACK").await;
    assert_eq!(
        ack.request().top_via().branch(),
        at_bob.request().top_via().branch(),
        "the ACK of a non-2xx rides the INVITE's own transaction (RFC 3261 §17.1.1.3)"
    );
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.resume(&p_ord, &b_ord);
    est.fh.advance(SETTLE).await;
    let (store, w_b1, w_b2, call_ref) = (&est.store, &est.w_b1, &est.w_b2, &est.call_ref);
    let drained = est
        .fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(call_ref).await
                && !w_b2.holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    est.fh.linger_peers(&[&est.alice, &est.bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[w_b1, w_b2], call_ref, store).await;
}
