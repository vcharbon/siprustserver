//! **A 2xx offer reaching a takeover copy's released round.** The proxy
//! misroutes the caller's offerless re-INVITE to the backup while the primary
//! lives (ADR-0014); the copy relays it, then ends the call on the caller's
//! BYE and releases itself. The callee's 2xx, carrying its offer, lands on
//! the orphaned client transaction, whose ACK must carry the answer (RFC 3261
//! §13.2.2.4): every stream rejected (RFC 3264 §6), in the session the stack
//! stated on the dialog, one version up (RFC 3264 §8, RFC 4566 §5.2).
//!
//! Beyond the failover sweep: a delayed-offer round crossing a copy's ending.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established};

const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";

/// The caller's offerless re-INVITE reaches the callee through the copy, and
/// the callee sends no provisional. The caller hangs up through the copy: it
/// answers her re-INVITE 487, holds the CANCEL of its relayed copy for a
/// provisional (RFC 3261 §9.1), relays the BYE and, once the callee answers
/// it, releases itself. The callee's 200 then lands on the orphan, whose ACK
/// answers its offer under the caller's origin, one up; no CANCEL leaves.
#[tokio::test(start_paused = true)]
async fn an_offer_in_a_2xx_after_the_copy_released_is_answered_in_the_dialogs_session() {
    let mut est = establish("offer-2xx-after-the-copy-ends").await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;

    let mut round = est.dialog.request(InDialogMethod::Invite, None).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    assert!(at_bob.request().body().is_empty(), "the offerless re-INVITE reached bob as it came");
    let mut bye = est.dialog.bye().await;
    bye.expect(200).await;
    round.expect(487).await;
    let mut bye_at_bob = est.bob.receive("BYE").await;
    let backup = if est.primary_ord == "b1" { &est.w_b2 } else { &est.w_b1 };
    let removed = backup.metrics().removals_total();
    bye_at_bob.respond(200, "OK").await;
    for _ in 0..200 {
        if backup.metrics().removals_total() > removed {
            break;
        }
        est.fh.advance(Duration::from_millis(1)).await;
    }
    assert!(!backup.serves(&est.call_ref), "the copy released itself on the BYE's 200");

    at_bob.respond(200, "OK").with_sdp(BOB_REOFFER).await;
    let ack = est.bob.receive("ACK").await;
    assert_eq!(ack.request().cseq().seq(), at_bob.request().cseq().seq());
    let answer = String::from_utf8_lossy(ack.request().body()).to_string();
    assert!(answer.contains("m=audio 0 "), "every offered stream rejected: {answer:?}");
    assert!(
        answer.contains("o=alice 1 2 IN IP4 127.0.0.1\r\n"),
        "the answer continues the session the stack stated to bob, one version up: {answer:?}"
    );

    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    settle(&est).await;
}

/// Nothing of the call is left anywhere, one CDR.
async fn settle(est: &Established) {
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
