//! A request the parser refuses is answered `400 Invalid Request` at ingress
//! when its Via / From / To / Call-ID / CSeq read (RFC 3261 §8.2.6): to the
//! packet's source, with no server transaction, every copy answered again,
//! and the ACK to the 400 drawing nothing. A response, an ACK and a request
//! no response can echo are dropped unanswered; the 400s are capped by a
//! token bucket, an answer it refuses dropped and counted.

use crate::common;
use common::*;
use sip_message::{CustomParser, HeaderName, SipMessage, SipParser, SipResponse};
use sip_txn::parse_refusal::PARSE_REFUSAL_BURST;
use sip_txn::TransactionConfig;

const TRANSIT: u64 = 5;

async fn stack() -> Stack {
    Stack::build(TRANSIT, 64, 1024).await
}

/// An INVITE whose Max-Forwards breaks the grammar; its other lines read.
fn malformed_invite(branch: &str, from: &str) -> Vec<u8> {
    format!(
        "INVITE sip:b2bua@127.0.0.1:5070 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: abc\r\n\
         From: {from}\r\n\
         To: <sip:b2bua@127.0.0.1:5070>\r\n\
         Call-ID: refused@10.0.0.1\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:caller@10.0.0.1:5555>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

const FROM: &str = "<sip:caller@10.0.0.1:5555>;tag=caller-tag";

/// The ACK to a 400 that carried `to_tag` (RFC 3261 §17.1.1.3).
fn ack_to_400(branch: &str, to_tag: &str) -> Vec<u8> {
    format!(
        "ACK sip:b2bua@127.0.0.1:5070 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: {FROM}\r\n\
         To: <sip:b2bua@127.0.0.1:5070>;tag={to_tag}\r\n\
         Call-ID: refused@10.0.0.1\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// Every datagram the peer received, as wire bytes.
fn drain_raw(stack: &Stack) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| stack.peer.try_recv()).map(|p| p.raw.to_vec()).collect()
}

fn response(raw: &[u8]) -> SipResponse {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Response(r)) => r,
        other => panic!("a response: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_malformed_invite_is_answered_400_each_copy_and_its_ack_draws_nothing() {
    let mut stack = stack().await;
    let invite = malformed_invite("z9hG4bK-bad-1", FROM);

    let mut tags = Vec::new();
    for copy in 1..=2 {
        stack.inject(&invite).await;
        elapse_ms(2 * TRANSIT + 1).await;
        let sent = drain_raw(&stack);
        assert_eq!(sent.len(), 1, "copy {copy} draws one answer");
        let resp = response(&sent[0]);
        assert_eq!((resp.status(), resp.reason()), (400, "Invalid Request"));
        assert_eq!(resp.top_via().branch(), Some("z9hG4bK-bad-1"));
        assert_eq!(resp.call_id().as_str(), "refused@10.0.0.1");
        let warning: Vec<_> = resp.raw_text(HeaderName::Warning).collect();
        assert_eq!(warning.len(), 1, "one Warning");
        assert!(
            warning[0].as_str().starts_with("399 127.0.0.1:5070 \"Invalid Max-Forwards"),
            "{}",
            warning[0].as_str()
        );
        tags.push(resp.to().tag().expect("a To-tag").to_string());
        assert!(stack.drain_events().is_empty(), "copy {copy} reaches no consumer");
        assert_eq!(stack.txn.metrics().active_transactions(), 0, "copy {copy} opens none");
    }
    assert_eq!(tags[0], tags[1], "every copy draws the same To-tag (§8.2.6.2)");

    stack.inject(&ack_to_400("z9hG4bK-bad-1", &tags[0])).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert!(drain_raw(&stack).is_empty(), "the ACK draws nothing");
    assert!(stack.drain_events().is_empty(), "the ACK to the layer's own 400 ends here");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "the ACK opens none");

    let m = stack.txn.metrics();
    assert_eq!(m.parse_refusals_answered(), 2);
    assert_eq!(m.parse_refusals_capped(), 0);
    assert_eq!(m.parse_errors(), 0, "no refusal dropped");
}

/// The warn-agent is the advertised address, not the bind address; the top
/// Via records the packet's source (RFC 3581 §4).
#[tokio::test(start_paused = true)]
async fn the_400_names_the_advertised_address_and_stamps_the_source() {
    let config =
        TransactionConfig { advertised: Some("b2bua.example.com:5070".to_string()), ..config() };
    let stack = Stack::build_with_config(TRANSIT, 64, config).await;
    let invite = String::from_utf8(malformed_invite("z9hG4bK-adv", FROM))
        .unwrap()
        .replace(";branch=z9hG4bK-adv", ";branch=z9hG4bK-adv;rport");
    stack.inject(invite.as_bytes()).await;
    elapse_ms(2 * TRANSIT + 1).await;
    let sent = drain_raw(&stack);
    assert_eq!(sent.len(), 1, "answered");
    let resp = response(&sent[0]);
    let warning: Vec<_> = resp.raw_text(HeaderName::Warning).collect();
    assert!(
        warning[0].as_str().starts_with("399 b2bua.example.com:5070 \""),
        "{}",
        warning[0].as_str()
    );
    assert_eq!(resp.top_via().rport(), Some(sip_message::header::Rport::Observed(5555)));
}

#[tokio::test(start_paused = true)]
async fn a_malformed_ack_is_dropped_unanswered() {
    let mut stack = stack().await;
    let mut ack = String::from_utf8(ack_to_400("z9hG4bK-bad-ack", "b2bua-tag")).unwrap();
    ack = ack.replace("Max-Forwards: 70", "Max-Forwards: abc");
    stack.inject(ack.as_bytes()).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert!(drain_raw(&stack).is_empty(), "an ACK is never answered");
    assert!(stack.drain_events().is_empty());
    assert_eq!(stack.txn.metrics().parse_errors(), 1);
    assert_eq!(stack.txn.metrics().parse_refusals_answered(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_malformed_response_is_dropped_unanswered() {
    let mut stack = stack().await;
    let resp = "SIP/2.0 486 Busy Here\r\n\
                Via: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-out\r\n\
                From: <sip:b2bua@127.0.0.1:5070>;tag=b2bua-tag\r\n\
                To: <sip:bob@10.0.0.1:5555>;tag=bob-tag\r\n\
                Call-ID: resp@127.0.0.1\r\n\
                CSeq: 1 INVITE\r\n\
                Expires: x\r\n\
                Content-Length: 0\r\n\r\n";
    stack.inject(resp.as_bytes()).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert!(drain_raw(&stack).is_empty(), "a response is never answered");
    assert!(stack.drain_events().is_empty());
    assert_eq!(stack.txn.metrics().parse_errors(), 1);
    assert_eq!(stack.txn.metrics().parse_refusals_answered(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_with_a_malformed_from_is_dropped_unanswered() {
    let mut stack = stack().await;
    stack.inject(&malformed_invite("z9hG4bK-bad-from", "<sip:caller@10.0.0.1>junk;tag=c")).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert!(drain_raw(&stack).is_empty(), "no From to echo: no answer");
    assert!(stack.drain_events().is_empty());
    assert_eq!(stack.txn.metrics().parse_errors(), 1);
    assert_eq!(stack.txn.metrics().parse_refusals_answered(), 0);
}

#[tokio::test(start_paused = true)]
async fn past_the_burst_a_refusal_is_dropped_and_counted() {
    let stack = stack().await;
    let burst = usize::try_from(PARSE_REFUSAL_BURST).unwrap();
    for i in 0..=burst {
        stack.inject(&malformed_invite(&format!("z9hG4bK-flood-{i}"), FROM)).await;
    }
    elapse_ms(2 * TRANSIT + 1).await;
    let sent = drain_raw(&stack);
    assert_eq!(sent.len(), burst, "the burst is answered, the one past it is not");
    let m = stack.txn.metrics();
    assert_eq!(m.parse_refusals_answered(), u64::from(PARSE_REFUSAL_BURST));
    assert_eq!(m.parse_refusals_capped(), 1);
    assert_eq!(m.parse_errors(), 0);
}

/// The ordinary counterpart: a well-formed INVITE / ACK / CANCEL draws no
/// 400 and moves no refusal counter.
#[tokio::test(start_paused = true)]
async fn well_formed_traffic_draws_no_400() {
    let mut stack = stack().await;
    stack.inject(&inbound_request("INVITE", "z9hG4bK-good", "good@10.0.0.1", None)).await;
    stack.inject(&inbound_request("CANCEL", "z9hG4bK-good", "good@10.0.0.1", None)).await;
    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-opt", "opt@10.0.0.1", None)).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert_eq!(count_responses(&stack.drain_peer(), 400), 0);
    assert!(!stack.drain_events().is_empty(), "the requests reach the consumer");
    let m = stack.txn.metrics();
    assert_eq!(
        (m.parse_errors(), m.parse_refusals_answered(), m.parse_refusals_capped()),
        (0, 0, 0)
    );
}
