//! RFC 3261 §9.1 / §17.1.2.2 — a CANCEL is a non-INVITE request and its wire
//! copy retransmits over UDP on the Timer-E ladder (T1, doubling, capped at
//! T2) until a CANCEL response arrives, the INVITE txn resolves, or the 64·T1
//! ceiling. CANCEL deliberately builds no client transaction of its own (it
//! reuses the INVITE's branch), so the ladder rides the INVITE client txn as a
//! sub-state — giving up at the ceiling abandons the CANCEL only, never an
//! INVITE txn still owing a final. A txn nothing ever answered is the one case
//! where the reverse holds: its Timer B (§17.1.1.2) ends both ladders at 64·T1.
//! ACK is exempt: a 2xx ACK is TU-owned and rides no timer (§13.2.2.4).

mod common;
use common::*;
use sip_message::SipMessage;
use sip_txn::{TimeoutKind, TransactionEvent, TxnKind};

/// The drained events contain the INVITE's final response of `status`.
fn has_final(events: &[TransactionEvent], status: u16) -> bool {
    events.iter().any(|e| match e {
        TransactionEvent::Message { message, .. } => {
            matches!(message.as_ref(), SipMessage::Response(r) if r.status() == status)
        }
        _ => false,
    })
}

#[tokio::test(start_paused = true)]
async fn cancel_retransmits_on_the_timer_e_ladder_to_the_64t1_ceiling() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ladder";

    // Out-of-dialog INVITE (long initial bound — the txn outlives the ladder)
    // that has its provisional, so the CANCEL passes straight to the wire and
    // the INVITE's own Timer A is already stopped (§17.1.1.2): every datagram
    // from here on is the CANCEL ladder's.
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 1);

    // t=0 of the ladder: the CANCEL's first wire copy.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);

    // The peer never answers the CANCEL (the lost-datagram scenario): the
    // ladder re-sends at T1, then doubling — assert each boundary exactly.
    for (quiet, label) in [
        (460, "T1"),      // fire at +500
        (960, "2*T1"),    // fire at +1500
        (1960, "4*T1"),   // fire at +3500
        (3960, "T2"),     // fire at +7500 (first T2-capped interval)
        (3960, "T2 plateau"), // fire at +11500 — stays at T2, not 8*T1
    ] {
        elapse_ms(quiet).await;
        assert_eq!(
            count_requests(&stack.drain_peer(), "CANCEL"),
            0,
            "no retransmit before the {label} deadline"
        );
        elapse_ms(40).await;
        assert_eq!(
            count_requests(&stack.drain_peer(), "CANCEL"),
            1,
            "exactly one retransmit at the {label} deadline"
        );
    }

    // The T2 plateau runs to the 64·T1 ceiling: fires at +15.5 s, +19.5 s,
    // +23.5 s, +27.5 s, +31.5 s — then the ladder gives up (the next fire
    // would land past 64·T1 = 32 s).
    elapse_ms(21_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 5, "the T2 plateau to the ceiling");
    elapse_ms(10_000).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "the ladder is silent past its 64*T1 ceiling"
    );
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 10);

    // The ceiling gave up on the CANCEL ONLY: the INVITE client txn is intact
    // (never displaced by the branch-sharing CANCEL) and still delivers its
    // final.
    assert_eq!(stack.txn.metrics().active_transactions(), 1);
    stack
        .inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true))
        .await;
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "non-2xx final auto-ACKed");
}

#[tokio::test(start_paused = true)]
async fn ladder_goes_quiescent_on_the_cancel_200() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ladder-200";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    stack.drain_peer();

    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    // First copy + the one T1 retransmit (the first copy was "lost").
    elapse_ms(540).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 2);

    // The 200 to the CANCEL stops the ladder immediately — quiescence.
    stack
        .inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20_000).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "no retransmit after the CANCEL is answered"
    );
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 1);

    // The INVITE txn was undisturbed throughout and still delivers its final.
    assert_eq!(stack.txn.metrics().active_transactions(), 1);
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
    assert!(has_final(&stack.drain_events(), 487), "the 487 final surfaced to the TU");
}

#[tokio::test(start_paused = true)]
async fn ladder_stops_when_the_invite_takes_its_final() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ladder-487";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    stack.drain_peer();

    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(540).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 2);

    // The 487 final resolves the INVITE txn (Completed, Timer-D hold): the
    // CANCEL ladder dies with it even though its 200 never arrived.
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
    assert!(has_final(&stack.drain_events(), 487));
    elapse_ms(20_000).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "the INVITE's final ends the CANCEL ladder"
    );
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 1);
}

#[tokio::test(start_paused = true)]
async fn grace_sent_cancel_rides_the_ladder_beside_timer_a() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ladder-grace";

    // Out-of-dialog INVITE toward a totally silent callee: Timer A keeps
    // re-ringing while the CANCEL waits out its grace window, then both
    // ladders run side by side on the one branch-keyed txn.
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    // 40 s covers both progressions in full:
    // - INVITE (Timer A, doubling): initial + 6 retransmits (0.5, 1.5, 3.5,
    //   7.5, 15.5, 31.5 s), then Timer B at 32 s;
    // - CANCEL: grace copy at 1 s, then the Timer-E ladder (0.5, 1.5, 3.5,
    //   7.5 s, then the T2 plateau) — 9 retransmits, the last at 28.5 s. Its
    //   own 64·T1 ceiling would land at 33 s, so on a callee that answers
    //   NOTHING the INVITE's Timer B is what ends the ladder, one rung short.
    elapse_ms(40_000).await;
    let msgs = stack.drain_peer();
    assert_eq!(count_requests(&msgs, "INVITE"), 7, "Timer A undisturbed beside the ladder");
    assert_eq!(count_requests(&msgs, "CANCEL"), 10, "grace copy + 9 ladder retransmits");
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 9);
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);

    // Timer B took the txn with it: nothing is left to ring, re-send or answer.
    // §17.1.1.2 scopes Timer B to Calling, and this branch never left it.
    assert!(
        stack.drain_events().iter().any(|e| matches!(
            e,
            TransactionEvent::Timeout { kind: TimeoutKind::Response, method: Some(m), .. } if m == "INVITE"
        )),
        "the unanswered INVITE times out on Timer B, not the long initial bound"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    elapse_ms(8_000).await;
    assert!(stack.drain_peer().is_empty(), "quiescent past both ceilings");
}

#[tokio::test(start_paused = true)]
async fn a_first_provisional_reflushes_the_grace_sent_cancel() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ladder-reflush";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // Grace expiry puts one CANCEL on the wire pre-1xx, then the ladder runs.
    elapse_ms(5_000).await;
    let before = count_requests(&stack.drain_peer(), "CANCEL");
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);

    // The first provisional: the UAS has its server transaction by now, so the
    // one owed pre-1xx copy is re-sent — and it is the SAME datagram, not a
    // restart of the ladder.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1, "the one owed re-send");
    assert_eq!(stack.txn.metrics().held_cancels_reflushed(), 1);
    assert!(before > 1, "the ladder was running before the provisional");

    // Complete the flow: the callee answers the CANCEL and rejects the INVITE.
    stack
        .inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true))
        .await;
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn ack_stays_off_the_ladder() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ack-raw";

    // A 2xx-bound ACK is TU-owned (§13.2.2.4): sent raw, once, no timers —
    // the CANCEL ladder must not have leaked onto ACK.
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(200, "OK", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    stack.drain_peer();

    stack
        .txn
        .send_request(outbound_request("ACK", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(40_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "one ACK, no retransmission");
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 0);
}
