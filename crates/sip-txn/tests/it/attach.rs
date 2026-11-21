//! A server transaction attributed to its call after it opened, as the
//! consumer resolves a request whose Request-URI names no call (an element on
//! its path rewrote it): from then on the call's books count it, the call's
//! release forgets or answers it, and its leaving takes it off the books, as
//! one the Request-URI attributed.

use crate::common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

/// A request of a dialog, attributed to `call_ref` by its Request-URI.
fn stamped_request(method: &str, branch: &str, call_ref: &str) -> Vec<u8> {
    format!(
        "{method} sip:b2bua@127.0.0.1:5070;callRef={call_ref} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: <sip:b2bua@127.0.0.1:5070>;tag=callee-tag\r\n\
         Call-ID: stamped@u\r\n\
         CSeq: 1 {method}\r\n\
         Contact: <sip:caller@10.0.0.1:5555>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn handed_up(events: &[TransactionEvent], method: &str) -> bool {
    events.iter().any(|e| match e {
        TransactionEvent::Message { message, .. } => {
            matches!(message.as_ref(), SipMessage::Request(r) if r.method() == method)
        }
        _ => false,
    })
}

/// An in-dialog INFO and re-INVITE whose Request-URI names no call are on no
/// call's books until the consumer attributes them; then the call counts
/// both, its release forgets the INFO and answers the re-INVITE 481 through
/// its transaction (§12.2.2), which absorbs the ACK. Forgotten and ACKed,
/// neither is on the call's books any more.
#[tokio::test(start_paused = true)]
async fn an_attached_transaction_is_reached_by_its_calls_release() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let info = inbound_request("INFO", "z9hG4bK-att-info", "rw@u", Some("callee-tag"));
    let reinvite = inbound_request("INVITE", "z9hG4bK-att-reinv", "rw@u", Some("callee-tag"));
    stack.inject(&info).await;
    stack.inject(&reinvite).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
    assert_eq!(stack.txn.active_txn_count_for_call("cr1").await.unwrap(), 0);

    stack.txn.attach_server_to_call(key_of(&info), "cr1").await.unwrap();
    stack.txn.attach_server_to_call(key_of(&reinvite), "cr1").await.unwrap();
    assert_eq!(stack.txn.active_txn_count_for_call("cr1").await.unwrap(), 2);

    assert_eq!(stack.txn.forget_unanswered_of_call("cr1").await.unwrap(), 1);
    let answered = stack
        .txn
        .answer_unanswered_invites_of_call("cr1", 481, "Call/Transaction Does Not Exist")
        .await
        .unwrap();
    assert_eq!(answered, 1);
    elapse_ms(60).await;
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 481), 1);
    assert_eq!(sent.len(), 1, "nothing else leaves");

    stack.inject(&info).await;
    elapse_ms(60).await;
    assert!(handed_up(&stack.drain_events(), "INFO"), "the forgotten INFO's copy is admitted");
    stack.inject(&inbound_request("ACK", "z9hG4bK-att-reinv", "rw@u", Some("callee-tag"))).await;
    elapse_ms(60).await;
    assert!(!handed_up(&stack.drain_events(), "ACK"), "the 481's transaction absorbs the ACK");
    assert_eq!(
        stack.txn.active_txn_count_for_call("cr1").await.unwrap(),
        0,
        "the forgotten INFO and the ACKed re-INVITE left the call's books"
    );
    assert_eq!(stack.txn.metrics().call_attach_refused(), 0);
}

/// The attribution a transaction holds is kept: one the Request-URI
/// attributed to a call is refused another and counted, and attaching it to
/// its own call again counts it once. A key naming no transaction and a
/// transaction that has sent its final are left as they are.
#[tokio::test(start_paused = true)]
async fn an_attribution_is_never_moved_and_an_answered_transaction_is_left_alone() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let stamped = stamped_request("INFO", "z9hG4bK-att-stamped", "cr1");
    let answered = "z9hG4bK-att-answered";
    let done = inbound_request("INFO", answered, "done@u", Some("callee-tag"));
    stack.inject(&stamped).await;
    stack.inject(&done).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let ok = parse_response(&response_bytes(200, "OK", "INFO", answered, "done@u", true));
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    stack.txn.attach_server_to_call(key_of(&stamped), "cr2").await.unwrap();
    assert_eq!(stack.txn.active_txn_count_for_call("cr2").await.unwrap(), 0);
    assert_eq!(stack.txn.metrics().call_attach_refused(), 1);
    stack.txn.attach_server_to_call(key_of(&stamped), "cr1").await.unwrap();
    assert_eq!(stack.txn.active_txn_count_for_call("cr1").await.unwrap(), 1);

    let unseen = inbound_request("INFO", "z9hG4bK-att-unseen", "unseen@u", Some("callee-tag"));
    stack.txn.attach_server_to_call(key_of(&unseen), "cr3").await.unwrap();
    stack.txn.attach_server_to_call(key_of(&done), "cr3").await.unwrap();
    assert_eq!(stack.txn.active_txn_count_for_call("cr3").await.unwrap(), 0);
    assert_eq!(stack.txn.metrics().call_attach_refused(), 1, "neither is a refusal");
}
