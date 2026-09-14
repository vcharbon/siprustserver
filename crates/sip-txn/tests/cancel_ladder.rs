//! The 487 the layer sends on its own for a CANCEL (RFC 3261 §9.2) holds the
//! INVITE server transaction the way a TU's non-2xx final does (§17.2.1): the
//! same datagram repeats on Timer G until the ACK, Timer H bounds it, and the
//! ACK ends the ladder and holds the branch for Timer I.

mod common;
use common::*;
use sip_message::SipMessage;
use sip_retransmit::Class;
use sip_txn::timers::TIMER_I;

const TRANSIT: u64 = 5;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// The bytes of every 487 in a drained batch.
fn images_of_487(out: &[SipMessage]) -> Vec<Vec<u8>> {
    out.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == 487 => Some(r.image().to_vec()),
            _ => None,
        })
        .collect()
}

/// INVITE in, a 180 out, a CANCEL in: the layer answers 200 + 487 itself.
async fn ringing_then_cancelled(stack: &mut Stack, branch: &str, call_id: &str) -> Vec<u8> {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let ringing = parse_response(&response_bytes(180, "Ringing", "INVITE", branch, call_id, true));
    stack.txn.send_response(ringing, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "the CANCEL is answered once: {out:?}");
    let sent = images_of_487(&out);
    assert_eq!(sent.len(), 1, "the 487 leaves once: {out:?}");
    let _ = stack.drain_events();
    sent.into_iter().next().unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_layers_own_487_repeats_on_timer_g_until_the_ack() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-cxl-ladder", "cxl-ladder");
    let first = ringing_then_cancelled(&mut stack, branch, call_id).await;
    assert_eq!(active(&stack), 1, "Completed on the 487 (Timer G/H)");

    // T1 after the final: the same datagram again.
    elapse_ms(600).await;
    let again = images_of_487(&stack.drain_peer());
    assert_eq!(again.len(), 1, "Timer G re-sends the 487 at T1");
    assert_eq!(again[0], first, "the repeat is the datagram that left");
    assert_eq!(stack.txn.metrics().retransmits(Class::InviteServerFinal), 1);

    // 2×T1 later: once more.
    elapse_ms(1100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 487), 1, "Timer G re-sends at 2×T1");

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(active(&stack), 1, "the ACK holds the branch in Confirmed");
    elapse_ms(TIMER_I - 100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 487), 0, "no repeat after the ACK");
    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "Timer I ends the transaction");
    let _ = stack.drain_events();
}

#[tokio::test(start_paused = true)]
async fn an_immediate_ack_leaves_the_487_sent_once() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-cxl-acked", "cxl-acked");
    let _ = ringing_then_cancelled(&mut stack, branch, call_id).await;

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    elapse_ms(4_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 487), 0, "the ACK ended the ladder");
    assert_eq!(stack.txn.metrics().retransmits(Class::InviteServerFinal), 0);
    let _ = stack.drain_events();
}

/// A request that named the dialog is answered under its own To-tag (RFC
/// 3261 §8.2.6.2): a re-INVITE cancelled before any provisional draws the
/// layer's 200 and 487 under the dialog's tag, that tag is what the layer
/// remembers past the transaction, and the TU's answer to a CANCEL
/// retransmitted after Timer I leaves under it untouched.
#[tokio::test(start_paused = true)]
async fn a_cancelled_in_dialog_invite_is_answered_and_remembered_under_its_own_tag() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id, tag) = ("z9hG4bK-cxl-in-dialog", "cxl-in-dialog", "dlg-tag");
    stack.inject(&inbound_request("INVITE", branch, call_id, Some(tag))).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();

    stack.inject(&inbound_request("CANCEL", branch, call_id, Some(tag))).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "{out:?}");
    assert_eq!(count_responses(&out, 487), 1, "{out:?}");
    for m in &out {
        if let SipMessage::Response(r) = m {
            assert_eq!(r.to().tag(), Some(tag), "answered under the request's own tag: {r:?}");
        }
    }
    let _ = stack.drain_events();

    stack.inject(&inbound_request("ACK", branch, call_id, Some(tag))).await;
    elapse_ms(TIMER_I + 100).await;
    assert_eq!(active(&stack), 0, "Timer I ended the transaction");
    let _ = stack.drain_peer();

    // The CANCEL again, its 200 lost: nothing matches it now, the TU answers.
    stack.inject(&inbound_request("CANCEL", branch, call_id, Some(tag))).await;
    elapse_ms(60).await;
    let events = stack.drain_events();
    let cancel = events
        .iter()
        .find_map(|e| match e {
            sip_txn::TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == "CANCEL" => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the retransmitted CANCEL reaches the TU");
    let refused = sip_message::generators::generate_response(
        &cancel,
        481,
        "Call/Transaction Does Not Exist",
        &sip_message::generators::GenerateResponseOpts::default(),
    );
    stack.txn.send_response(refused, addr(PEER)).await.unwrap();
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let refusals: Vec<_> = out
        .iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == 481 => Some(r.to().tag().map(str::to_string)),
            _ => None,
        })
        .collect();
    assert_eq!(refusals, [Some(tag.to_string())], "the 481 keeps the dialog's tag: {out:?}");
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_coerced(), 0, "nothing was re-rendered");
    assert_eq!(m.to_tag_filled(), 0);
}
