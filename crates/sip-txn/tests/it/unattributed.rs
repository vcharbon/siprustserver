//! An in-dialog request whose Request-URI names no call (an element on its
//! path rewrote it) opens a server transaction attributed to no call. Until
//! the consumer attributes it, the count of a call whose legs' Call-IDs name
//! its Call-ID counts it, so a takeover copy is never released between the
//! request's arrival and its attribution (ADR-0014).

use crate::common::*;
use sip_txn::timers::{TIMER_J, TIMER_L};

const TRANSIT: u64 = 5;

/// The transactions serving `call_ref`, whose legs' Call-IDs are `call_ids`.
async fn count(stack: &Stack, call_ref: &str, call_ids: &[&str]) -> usize {
    let call_ids: Vec<String> = call_ids.iter().map(|id| id.to_string()).collect();
    stack.txn.active_txn_count_for_call(call_ref, &call_ids).await.unwrap()
}

/// The TU answers 200 to the in-dialog `method` on `branch` of `call_id`
/// ([`inbound_request`]'s dialog), echoing its tags.
async fn respond(stack: &Stack, method: &str, branch: &str, call_id: &str) {
    let bytes = format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: <sip:b2bua@127.0.0.1:5070>;tag=callee-tag\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {method}\r\n\
         Contact: <sip:b2bua@{B2BUA}>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    stack.txn.send_response(parse_response(bytes.as_bytes()), addr(PEER)).await.unwrap();
}

/// An in-dialog INFO naming no call is counted for the call whose Call-IDs
/// name its own, once however often they name it, and for no other; a
/// request outside any dialog is counted for none. Attributed to the call,
/// it is counted once, on its books, and leaves them at Timer J's end.
#[tokio::test(start_paused = true)]
async fn an_unattributed_request_serves_the_call_naming_its_call_id_until_it_leaves() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-una-info";
    let info = inbound_request("INFO", branch, "rw@u", Some("callee-tag"));
    stack.inject(&info).await;
    stack.inject(&inbound_request("MESSAGE", "z9hG4bK-una-ood", "ood@u", None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    assert_eq!(count(&stack, "cr1", &["a@u", "rw@u"]).await, 1, "counted before its attribution");
    assert_eq!(count(&stack, "cr1", &["rw@u", "rw@u"]).await, 1, "a Call-ID named twice");
    assert_eq!(count(&stack, "cr1", &["a@u", "b@u"]).await, 0, "another call's Call-IDs");
    assert_eq!(count(&stack, "cr1", &[]).await, 0, "no Call-ID named");
    assert_eq!(count(&stack, "cr1", &["ood@u"]).await, 0, "a request outside any dialog");

    stack.txn.attach_server_to_call(key_of(&info), "cr1").await.unwrap();
    assert_eq!(count(&stack, "cr1", &["a@u", "rw@u"]).await, 1, "counted once after it");
    respond(&stack, "INFO", branch, "rw@u").await;
    elapse_ms(60).await;
    assert_eq!(count(&stack, "cr1", &["rw@u"]).await, 1, "on the books until Timer J ends");
    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(count(&stack, "cr1", &["rw@u"]).await, 0, "gone with its transaction");
}

/// An unattributed transaction stops serving the call once it has sent its
/// final, a re-INVITE's 2xx or an INFO's 200, and leaves for good with the
/// end of its hold (Timer L, Timer J), its forget by the consumer, or its
/// relinquish.
#[tokio::test(start_paused = true)]
async fn an_unattributed_request_leaves_the_count_on_every_way_out() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let ids = ["l@u", "j@u", "f@u", "r@u"];
    let info = |branch, call_id| inbound_request("INFO", branch, call_id, Some("callee-tag"));
    let accepted = inbound_request("INVITE", "z9hG4bK-una-l", "l@u", Some("callee-tag"));
    let answered = info("z9hG4bK-una-j", "j@u");
    let forgotten = info("z9hG4bK-una-f", "f@u");
    let relinquished = info("z9hG4bK-una-r", "r@u");
    for request in [&accepted, &answered, &forgotten, &relinquished] {
        stack.inject(request).await;
    }
    elapse_ms(60).await;
    let _ = stack.drain_events();
    assert_eq!(count(&stack, "cr1", &ids).await, 4);

    respond(&stack, "INVITE", "z9hG4bK-una-l", "l@u").await;
    elapse_ms(5_000).await;
    respond(&stack, "INFO", "z9hG4bK-una-j", "j@u").await;
    stack.txn.forget_unanswered(&key_of(&forgotten), "f@u", "caller-tag");
    assert!(stack.txn.relinquish_server(key_of(&relinquished)).await.unwrap());
    elapse_ms(60).await;
    assert_eq!(count(&stack, "cr1", &ids).await, 0, "answered, forgotten or relinquished");

    elapse_ms(TIMER_L - 5_000).await;
    assert_eq!(count(&stack, "cr1", &ids).await, 0, "the re-INVITE left at Timer L's end");
    elapse_ms(5_000).await;
    assert_eq!(count(&stack, "cr1", &ids).await, 0, "the INFO left at Timer J's end");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "none is resident");
}
