//! **A live primary the proxy reads dead, the dead verdict told.** The proxy
//! misreads the primary of an established call dead while it runs, and tells
//! the backup (ADR-0050 §3). The replication stream from the primary stays
//! open, so the backup takes nothing over at either verdict (ADR-0047 D1).
//! The caller's re-INVITE, which the proxy now routes to the backup, is
//! relayed by a copy there. The proxy reads the primary alive, the callee's
//! INFO goes through it, and the copy stands down on the primary's write
//! (ADR-0047 D7). The proxy reads the primary dead again, and that verdict
//! takes nothing over either. The callee's 200, which still reaches the
//! backup, takes the call over afresh: the round completes end to end; the
//! call ends with the caller's BYE and one CDR. The twin of
//! `stood_down_round.rs`, which tells no verdict.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};

#[tokio::test(start_paused = true)]
async fn a_dead_verdict_on_a_live_primary_takes_nothing_over_and_the_callees_answer_does() {
    let name = "flap-read-dead-takes-the-round-over";
    let Established {
        fh,
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
    } = establish(name).await;
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };

    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    assert!(!backup.serves(&call_ref), "a quiet call is not taken over at the verdict");
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    round.expect(100).await;
    fh.advance(Duration::from_millis(300)).await;
    assert!(backup.serves(&call_ref), "the backup's copy serves the round");

    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let hydrated = backup.metrics().repl_takeover_hydrated_total();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(backup.metrics().repl_stand_down_total(), 1, "the copy stood down");
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 2, "the second verdict is told");
    assert_eq!(
        backup.metrics().repl_takeover_hydrated_total(),
        hydrated,
        "the second verdict takes nothing over"
    );
    assert!(!backup.serves(&call_ref), "the copy stood down");

    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    dialog.ack(None).await;
    bob.receive_ack_of(at_bob.request().cseq().seq()).await;
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
    fh.assert_sip_rfc_clean(name);
}
