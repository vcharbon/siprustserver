//! **A re-INVITE 2xx a takeover copy relayed, ACKed through the primary.** The
//! proxy misroutes the caller's re-INVITE to the backup while the primary
//! lives (ADR-0014); the backup's copy relays it and the callee holds it. The
//! callee's 2xx follows the copy's Via back to the backup, which relays it to
//! the caller; the caller's ACK is a new request the proxy routes on its
//! health view, which reads the primary alive again. RFC 3261 §13.2.2.4 owes
//! the callee exactly one ACK for its 2xx, whichever node the caller's ACK
//! reaches; the call then ends with a BYE and settles to one CDR.
//!
//! Two timing axes:
//! - `PrimaryWrite`: whether the primary writes the call (an INFO the callee
//!   sends through it) while the round is open, which stands the copy down
//!   and makes the callee's 2xx take the call over afresh (ADR-0047 D7), or
//!   after the 2xx reached the caller and before the caller's ACK.
//! - `Heal`: whether the proxy reads the primary alive before the callee
//!   answers, the caller ACKing at once, or only after the answer, the
//!   replication settling before the caller ACKs.
//!
//! The ACK reaching the backup instead is `stood_down_round` (mid-round write)
//! and `call_terminate_on_backup::c11_reinvite_on_backup__primary_alive__no_terminal`.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::{Method, SipMessage};

use crate::call_terminate_on_backup::{
    accept_takeover_cseq_overlap, establish, Established, ANSWER, OFFER,
};

/// Whether, and when, the primary writes the call before the caller's ACK.
#[derive(Clone, Copy, PartialEq)]
enum PrimaryWrite {
    /// Nothing reaches the primary until the caller's ACK.
    None,
    /// The callee sends an INFO through the primary while the round is open.
    InRound,
    /// The callee sends an INFO through the primary after its 2xx reached the
    /// caller, before the caller's ACK.
    AfterAnswer,
}

/// When the proxy reads the primary alive again.
#[derive(Clone, Copy)]
enum Heal {
    /// Before the callee's 2xx; the caller ACKs as soon as the 2xx reaches it.
    BeforeAnswer,
    /// After the callee's 2xx has reached the caller; the caller ACKs once
    /// the replication between the two nodes has settled.
    AfterAnswer,
}

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// Run one cell: a misrouted re-INVITE round on an established call, its 2xx
/// relayed by the backup, the caller's ACK routed to the primary; then a BYE.
async fn reinvite_2xx_acked_through_the_primary(name: &str, write: PrimaryWrite, heal: Heal) {
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
    } = establish(name).await;
    accept_takeover_cseq_overlap(&mut fh);
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };

    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    round.expect(100).await;
    fh.advance(Duration::from_millis(300)).await;
    assert!(backup.serves(&call_ref), "the backup's copy serves the round");

    if write == PrimaryWrite::InRound {
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
        info_through_the_primary(&alice, &mut bob_dialog).await;
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        fh.advance(SETTLE).await;
        assert!(!backup.serves(&call_ref), "the copy stands down on the primary's write");
    }

    if let Heal::BeforeAnswer = heal {
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
        fh.advance(Duration::from_millis(200)).await;
    }
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    if let Heal::AfterAnswer = heal {
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
        fh.advance(SETTLE).await;
    }
    if write == PrimaryWrite::AfterAnswer {
        info_through_the_primary(&alice, &mut bob_dialog).await;
        fh.advance(SETTLE).await;
    }
    // The re-INVITE carried the offer, the 2xx the answer: a bodyless ACK.
    dialog.ack(None).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_secs(2)).await;
    assert_no_further_ack(&bob).await;

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

/// The callee sends an INFO the proxy routes to the primary; the caller
/// answers it 200.
async fn info_through_the_primary(alice: &Agent, bob_dialog: &mut Dialog) {
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
}

/// The callee drew one ACK for its 2xx (RFC 3261 §13.2.2.4): nothing queued
/// at it since is a second.
async fn assert_no_further_ack(bob: &Agent) {
    while let Some(msg) = bob.take_queued().await {
        let ack = matches!(&msg, SipMessage::Request(r) if *r.method() == Method::Ack);
        assert!(!ack, "the callee draws exactly one ACK for its 2xx");
    }
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_relayed_by_the_copy_is_acked_once_through_the_primary_healed_before_the_answer(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-quiet-heal-before",
        PrimaryWrite::None,
        Heal::BeforeAnswer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_relayed_by_the_copy_is_acked_once_through_the_primary_healed_after_the_answer(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-quiet-heal-after",
        PrimaryWrite::None,
        Heal::AfterAnswer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_relayed_by_a_copy_taken_over_afresh_is_acked_once_through_the_primary_healed_before_the_answer(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-written-heal-before",
        PrimaryWrite::InRound,
        Heal::BeforeAnswer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_relayed_by_a_copy_taken_over_afresh_is_acked_once_through_the_primary_healed_after_the_answer(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-written-heal-after",
        PrimaryWrite::InRound,
        Heal::AfterAnswer,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_relayed_by_the_copy_is_acked_once_through_the_primary_written_after_the_answer(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-written-after-answer",
        PrimaryWrite::AfterAnswer,
        Heal::AfterAnswer,
    )
    .await;
}
