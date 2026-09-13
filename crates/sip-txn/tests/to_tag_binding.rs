//! The To-tag every outbound response is held to (RFC 3261 §8.2.6.2, §9.2,
//! §12.1.1): the final to an INVITE binds this node's tag to the dialog, and
//! nothing the TU hands over for that transaction or its CANCEL leaves under
//! another one — re-rendered and counted where the TU chose wrong, filled
//! where it chose nothing, untouched where nothing is bound.

mod common;
use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest};
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};
use std::sync::Arc;

const TRANSIT: u64 = 5;

/// A TU response on `branch`, `tag` on its To where given.
fn tu_response(
    status: u16,
    cseq_method: &str,
    branch: &str,
    call_id: &str,
    tag: Option<&str>,
) -> Vec<u8> {
    let to = match tag {
        Some(t) => format!("<sip:peer@10.0.0.1:5555>;tag={t}"),
        None => "<sip:peer@10.0.0.1:5555>".to_string(),
    };
    format!(
        "SIP/2.0 {status} Reason\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {cseq_method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

async fn send(stack: &Stack, raw: Vec<u8>) {
    stack.txn.send_response(parse_response(&raw), addr(PEER)).await.unwrap();
    elapse_ms(20).await;
}

/// The one request of `method` the TU was handed, out of the drained events.
fn handed_up(events: &[TransactionEvent], method: &str) -> SipRequest {
    let reqs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(reqs.len(), 1, "one {method} handed to the TU: {events:?}");
    reqs[0].clone()
}

/// The To-tags of every response of `status` in a drained batch.
fn to_tags(msgs: &[SipMessage], status: u16) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == status => {
                Some(r.to().tag().unwrap_or("").to_string())
            }
            _ => None,
        })
        .collect()
}

/// INVITE in, 200 under `tag` out, the caller's ACK in: the transaction is
/// gone and the dialog's tag is only remembered.
async fn answered_and_acked(stack: &mut Stack, branch: &str, call_id: &str, tag: &str) {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(20).await;
    send(stack, tu_response(200, "INVITE", branch, call_id, Some(tag))).await;
    stack.inject(&inbound_request("ACK", branch, call_id, Some(tag))).await;
    elapse_ms(20).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
}

#[tokio::test(start_paused = true)]
async fn a_tagless_answer_to_a_cancel_after_the_ack_takes_the_dialogs_tag() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-bind-a", "bind-a");
    answered_and_acked(&mut stack, branch, call_id, "dlg-a").await;

    // The CANCEL matches no transaction now and reaches the TU, which answers
    // it 481 knowing no tag — the generator's fallback.
    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let cancel = handed_up(&stack.drain_events(), "CANCEL");
    let answer = generate_response(&cancel, 481, "Gone", &GenerateResponseOpts::default());
    assert!(
        sip_message::generators::response::is_fallback_to_tag(answer.to().tag().unwrap()),
        "the TU minted nothing: {:?}",
        answer.to().tag()
    );
    let sent = stack.txn.send_response(answer, addr(PEER)).await.unwrap();
    elapse_ms(20).await;

    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 481), ["dlg-a"], "the 481 carries the 2xx's tag (§9.2): {out:?}");
    assert_eq!(parse_response(&sent).to().tag(), Some("dlg-a"), "what left is what was returned");
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_filled(), 1);
    assert_eq!(m.to_tag_coerced(), 0);
    assert_eq!(m.fallback_to_tag_used(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_wrong_tag_on_a_cancel_answer_is_re_rendered_under_the_bound_one() {
    let mut stack = Stack::build_with_config(
        TRANSIT,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            strict_to_tag: false,
            ..Default::default()
        },
    )
    .await;
    let (branch, call_id) = ("z9hG4bK-bind-b", "bind-b");
    answered_and_acked(&mut stack, branch, call_id, "dlg-b").await;

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let _ = handed_up(&stack.drain_events(), "CANCEL");
    let sent = stack
        .txn
        .send_response(
            parse_response(&tu_response(481, "CANCEL", branch, call_id, Some("wrong"))),
            addr(PEER),
        )
        .await
        .unwrap();
    elapse_ms(20).await;

    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 481), ["dlg-b"], "the wire carries the bound tag: {out:?}");
    assert_eq!(parse_response(&sent).to().tag(), Some("dlg-b"));
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_coerced(), 1, "the TU's defect is counted");
    assert_eq!(m.to_tag_filled(), 0);
}

#[tokio::test(start_paused = true)]
async fn forks_open_freely_and_the_final_binds_the_cancel_answer() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-bind-c", "bind-c");
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(20).await;
    let _ = stack.drain_peer();

    send(&stack, tu_response(183, "INVITE", branch, call_id, Some("t1"))).await;
    send(&stack, tu_response(180, "INVITE", branch, call_id, Some("t2"))).await;
    send(&stack, tu_response(200, "INVITE", branch, call_id, Some("t3"))).await;
    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 183), ["t1"]);
    assert_eq!(to_tags(&out, 180), ["t2"]);
    assert_eq!(to_tags(&out, 200), ["t3"], "every early dialog and the final keep their tag");

    // The CANCEL still matches the held transaction: the layer answers it
    // itself, under the final's tag — not the first provisional's.
    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 200), ["t3"], "the CANCEL's 200 carries the final's tag: {out:?}");
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_coerced(), 0);
    assert_eq!(m.to_tag_filled(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_before_the_final_is_answered_under_the_first_provisionals_tag() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-bind-f", "bind-f");
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(20).await;
    let _ = stack.drain_peer();

    // Two early dialogs mirrored from a forking downstream.
    send(&stack, tu_response(180, "INVITE", branch, call_id, Some("t1"))).await;
    send(&stack, tu_response(180, "INVITE", branch, call_id, Some("t2"))).await;
    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 180), ["t1", "t2"], "each early dialog keeps its tag");

    // No final yet: the tag pinned on the first response is the transaction's
    // identity toward the requester (§8.2.6.2, §17.2.1); the CANCEL's 200 and
    // the 487 both carry it (§9.2), not the newest early dialog's.
    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 200), ["t1"], "the CANCEL's 200 carries the first tag: {out:?}");
    assert_eq!(to_tags(&out, 487), ["t1"], "the 487 carries the first tag: {out:?}");
    stack.inject(&inbound_request("ACK", branch, call_id, Some("t1"))).await;
    elapse_ms(20).await;
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_coerced(), 0);
    assert_eq!(m.to_tag_filled(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_in_dialog_answer_echoes_the_requests_tag_untouched() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-bind-d", "bind-d");
    answered_and_acked(&mut stack, branch, call_id, "dlg-d").await;

    let bye_branch = "z9hG4bK-bind-d-bye";
    stack.inject(&inbound_request("BYE", bye_branch, call_id, Some("dlg-d"))).await;
    elapse_ms(20).await;
    let bye = handed_up(&stack.drain_events(), "BYE");
    let answer = generate_response(&bye, 200, "OK", &GenerateResponseOpts::default());
    let sent = stack.txn.send_response(answer, addr(PEER)).await.unwrap();
    elapse_ms(20).await;

    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 200), ["dlg-d"], "echoed as it came: {out:?}");
    assert_eq!(parse_response(&sent).to().tag(), Some("dlg-d"));
    let m = stack.txn.metrics();
    assert_eq!(m.to_tag_coerced(), 0);
    assert_eq!(m.to_tag_filled(), 0);
    assert_eq!(m.fallback_to_tag_used(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_fallback_tag_nothing_binds_leaves_and_is_counted() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-bind-e", "bind-e");
    // A CANCEL for an INVITE this node never saw: no transaction, nothing
    // remembered — the TU's fallback is all there is.
    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let cancel = handed_up(&stack.drain_events(), "CANCEL");
    let answer = generate_response(&cancel, 481, "Gone", &GenerateResponseOpts::default());
    let fallback = answer.to().tag().unwrap().to_string();
    stack.txn.send_response(answer, addr(PEER)).await.unwrap();
    elapse_ms(20).await;

    let out = stack.drain_peer();
    assert_eq!(to_tags(&out, 481), [fallback]);
    let m = stack.txn.metrics();
    assert_eq!(m.fallback_to_tag_used(), 1);
    assert_eq!(m.to_tag_filled(), 0);
    assert_eq!(m.to_tag_coerced(), 0);
}
