//! RFC 6026 §7.2, §8.4: a 2xx an INVITE client transaction owes nothing. In
//! Completed, past a non-2xx final, the transaction takes only that final's
//! repeats; a re-INVITE that gave up at Timer B with no response is over. The
//! layer absorbs such a 2xx: no ACK, nothing handed up, counted. A dialling
//! INVITE's Timer B hold still hands its late 2xx up, for the TU to ACK and
//! end the dialog it confirms.

use crate::common;

use call::TxnKind;
use common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

/// Past Timer B (64·T1).
const PAST_TIMER_B_MS: u64 = 32_100;

/// The `status` responses handed up, with whether the layer matched them.
fn handed_up(events: &[TransactionEvent], status: u16) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                match message.as_ref() {
                    SipMessage::Response(r) if r.status() == status => Some(*matched_client_txn),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// A response from the peer to [`outbound_reinvite`] on `branch`: its
/// Call-ID and its CSeq.
fn reinvite_response(status: u16, reason: &str, branch: &str) -> Vec<u8> {
    let raw = response_bytes(status, reason, "INVITE", branch, "reinvite-test", true);
    String::from_utf8_lossy(&raw).replace("CSeq: 1 INVITE", "CSeq: 2 INVITE").into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_2xx_after_a_non_2xx_final_is_absorbed() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_id, branch) = ("callid-rejected-then-2xx", "z9hG4bK-rejected-then-2xx");
    let invite = invite_with_cr_lg("self|call-rejected", call_id, branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack
        .inject(&response_bytes(488, "Not Acceptable Here", "INVITE", branch, call_id, true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "the 488 is hop-ACKed");
    assert_eq!(handed_up(&stack.drain_events(), 488), vec![true]);

    let ok = response_bytes(200, "OK", "INVITE", branch, call_id, true);
    stack.inject(&ok).await;
    stack.inject(&ok).await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "no ACK answers the 2xx");
    assert_eq!(handed_up(&stack.drain_events(), 200), Vec::<bool>::new(), "nothing is handed up");
    assert_eq!(stack.txn.metrics().late_2xx_dropped(), 2, "the 2xx and its copy are counted");
}

#[tokio::test(start_paused = true)]
async fn a_2xx_to_a_reinvite_past_its_timer_b_is_absorbed() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-reinvite-timer-b";
    stack.txn.send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(PAST_TIMER_B_MS).await;
    let _ = stack.drain_peer();
    let timed_out = stack.drain_events();
    assert!(timed_out.iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })));

    stack.inject(&reinvite_response(200, "OK", branch)).await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "no ACK answers the 2xx");
    assert_eq!(handed_up(&stack.drain_events(), 200), Vec::<bool>::new(), "nothing is handed up");
    assert_eq!(stack.txn.metrics().late_2xx_dropped(), 1);
}

/// The dialog a dialling INVITE's late 2xx confirms is the TU's to ACK and
/// end: its hold hands that 2xx up, matched.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_a_dialling_invite_past_its_timer_b_is_handed_up() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_id, branch) = ("callid-dialling-timer-b", "z9hG4bK-dialling-timer-b");
    let invite = invite_with_cr_lg("self|call-dialling", call_id, branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(PAST_TIMER_B_MS).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();

    stack.inject(&response_bytes(200, "OK", "INVITE", branch, call_id, true)).await;
    elapse_ms(20).await;
    assert_eq!(handed_up(&stack.drain_events(), 200), vec![true]);
    assert_eq!(stack.txn.metrics().late_2xx_dropped(), 0);
}
