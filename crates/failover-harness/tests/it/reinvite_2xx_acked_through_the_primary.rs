//! **A re-INVITE 2xx a takeover copy relayed, ACKed through the primary.** The
//! proxy misroutes the caller's re-INVITE to the backup while the primary
//! lives (ADR-0014); the backup's copy relays it and the callee holds it. The
//! callee's 2xx follows the copy's Via back to the backup, which relays it to
//! the caller; the caller's ACK is a new request the proxy routes on its
//! health view, which reads the primary alive again. RFC 3261 §13.2.2.4 owes
//! the callee exactly one ACK for its 2xx, whichever node the caller's ACK
//! reaches; the caller re-ACKs every copy of the 2xx that reaches her, so an
//! ACK the primary drops before it holds the round is recovered by the next
//! copy. The call then ends with a BYE and settles to one CDR.
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

use failover_harness::{assert_call_fully_over, FailoverHarness, WorkerHealth};
use scenario_harness::{Agent, Dialog, Inbound};
use sip_message::generators::InDialogMethod;
use sip_message::{Method, SipMessage, SipResponse};

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};

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

/// How long the caller keeps re-ACKing copies of the 2xx after her ACK: past
/// the third rung of the §13.3.1.4 ladder (T1, 2·T1, 4·T1).
const ACK_WINDOW: Duration = Duration::from_secs(4);

/// The clock step the caller and the callee read their sockets at.
const STEP: Duration = Duration::from_millis(100);

/// Run one cell: a misrouted re-INVITE round on an established call, its 2xx
/// relayed by the backup, the caller's ACK routed to the primary; then a BYE.
async fn reinvite_2xx_acked_through_the_primary(name: &str, write: PrimaryWrite, heal: Heal) {
    let Established {
        fh,
        alice,
        bob,
        mut proxy,
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
    // The reactive path: the proxy tells no dead verdict (ADR-0050 §3).
    proxy.tell_no_dead_verdict();
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };

    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = bob.receive("INVITE").await;
    let round_cseq = at_bob.request().cseq().seq();
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
    let acks = ack_the_answer(&fh, &alice, &mut dialog, &bob, round_cseq).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");

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
/// answers it 200. A copy of the 2xx that reaches her first is acknowledged by
/// the ACK she sends next.
async fn info_through_the_primary(alice: &Agent, bob_dialog: &mut Dialog) {
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    loop {
        match alice.recv_any().await.expect("the INFO reaches the caller") {
            Inbound::Request(mut t) if t.request().method() == "INFO" => {
                t.respond(200, "OK").await;
                break;
            }
            Inbound::Response(r) if copy_of_the_answer(&r) => {}
            Inbound::Request(t) => panic!("unexpected {} toward the caller", t.request().method()),
            Inbound::Response(r) => panic!("unexpected {} toward the caller", r.status()),
        }
    }
    info.expect(200).await;
}

/// The caller ACKs the 2xx — the re-INVITE carried the offer, the 2xx the
/// answer, so the ACK is bodyless — then re-ACKs its copies
/// ([`re_ack_copies`]). Returns the ACKs of the round the callee drew
/// meanwhile.
pub(crate) async fn ack_the_answer(
    fh: &FailoverHarness,
    alice: &Agent,
    dialog: &mut Dialog,
    bob: &Agent,
    round_cseq: u32,
) -> usize {
    dialog.ack(None).await;
    re_ack_copies(fh, alice, dialog, bob, round_cseq).await
}

/// The caller re-ACKs every copy of the 2xx that reaches her for
/// [`ACK_WINDOW`] (RFC 3261 §13.2.2.4). Returns the ACKs of the round the
/// callee answered at `round_cseq` it drew meanwhile; an ACK of an earlier
/// 2xx a copy re-sends ahead of its seed is not one, and nothing else reaches
/// either party.
pub(crate) async fn re_ack_copies(
    fh: &FailoverHarness,
    alice: &Agent,
    dialog: &mut Dialog,
    bob: &Agent,
    round_cseq: u32,
) -> usize {
    let mut acks = 0;
    let mut waited = Duration::ZERO;
    while waited < ACK_WINDOW {
        fh.advance(STEP).await;
        waited += STEP;
        while let Some(msg) = alice.take_queued().await {
            match msg {
                SipMessage::Response(r) if copy_of_the_answer(&r) => dialog.ack(None).await,
                other => panic!("unexpected toward the caller: {other:?}"),
            }
        }
        while let Some(msg) = bob.take_queued().await {
            match msg {
                SipMessage::Request(r) if *r.method() == Method::Ack => {
                    acks += usize::from(r.cseq().seq() == round_cseq);
                }
                other => panic!("unexpected toward the callee: {other:?}"),
            }
        }
    }
    acks
}

/// A copy of the re-INVITE's 2xx (RFC 3261 §13.3.1.4).
fn copy_of_the_answer(r: &SipResponse) -> bool {
    r.status() == 200 && *r.cseq().method() == Method::Invite
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
async fn a_reinvite_2xx_relayed_by_the_copy_is_acked_once_through_the_primary_written_after_the_answer_healed_before_it(
) {
    reinvite_2xx_acked_through_the_primary(
        "reinvite-2xx-acked-at-primary-written-after-answer-heal-before",
        PrimaryWrite::AfterAnswer,
        Heal::BeforeAnswer,
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
