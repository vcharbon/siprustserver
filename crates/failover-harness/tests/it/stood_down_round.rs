//! **A takeover copy stood down in the middle of a round it opened.** The
//! proxy misroutes the caller's re-INVITE to the backup while the primary
//! lives (ADR-0014); the backup's copy relays it. The primary then writes the
//! call (an INFO the callee sends through it), having read the copy's write,
//! so the copy stands down with the round still open and its transactions
//! relinquished (ADR-0047 D7). The callee's 200, which still reaches the
//! backup, takes the call over afresh: the round completes end to end and the
//! call settles to one CDR.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{
    accept_takeover_cseq_overlap, establish, Established, ANSWER, OFFER,
};

#[tokio::test(start_paused = true)]
async fn a_copy_stood_down_mid_round_takes_the_call_over_afresh_on_the_callees_answer() {
    let Established {
        mut fh,
        alice,
        bob,
        proxy,
        store,
        lh: _lh,
        w_b1,
        w_b2,
        primary_ord,
        call_ref,
        mut dialog,
        mut bob_dialog,
        ..
    } = establish("copy-stood-down-mid-round").await;
    accept_takeover_cseq_overlap(&mut fh);
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };

    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    round.expect(100).await;
    fh.advance(Duration::from_millis(300)).await;
    assert!(backup.serves(&call_ref), "the backup's copy serves the round");

    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(500)).await;
    assert!(!backup.serves(&call_ref), "the copy stands down on the primary's write");
    assert_eq!(backup.metrics().repl_stand_down_total(), 1);

    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}
