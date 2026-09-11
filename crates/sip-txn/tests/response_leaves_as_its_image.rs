//! ADR-0029 X3 — a response leaves the transaction layer as its own image,
//! byte for byte. A TU that retains `image()` for a §13.3.1.4 or RFC 3262 §3
//! repeat therefore retains what went on the wire, by construction: there is
//! one rendering (at parse or at freeze) and two holders of it, never a
//! second render at the socket.

mod common;
use common::*;
use sip_message::header::HeaderName;

const TRANSIT_MS: u64 = 5;

/// Every datagram the peer has received from the b2bua, raw and in order.
fn raw_from_b2bua(stack: &Stack) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(p) = stack.peer.try_recv() {
        out.push(p.raw);
    }
    out
}

/// An INVITE server transaction, its auto-100 already drained, so the next
/// datagram the peer sees is the TU's response.
async fn admitted_invite(branch: &str, call_id: &str) -> Stack {
    let mut stack = Stack::build(TRANSIT_MS, 64, 64).await;
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(TRANSIT_MS * 4).await;
    let _ = stack.drain_events();
    let _ = raw_from_b2bua(&stack);
    stack
}

/// A parsed response goes out as the datagram it was parsed from.
#[tokio::test(start_paused = true)]
async fn a_parsed_response_leaves_as_the_datagram_it_was_parsed_from() {
    let stack = admitted_invite("z9hG4bK-img-parsed", "img-parsed").await;
    let resp = parse_response(&response_bytes(
        200,
        "OK",
        "INVITE",
        "z9hG4bK-img-parsed",
        "img-parsed",
        true,
    ));
    let image = resp.image().to_vec();

    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(TRANSIT_MS * 4).await;

    assert_eq!(raw_from_b2bua(&stack), vec![image], "the wire bytes are the response's image");
}

/// A response the TU edited (thawed, a header pushed, frozen) goes out as the
/// image the freeze rendered — the edit is on the wire, and so is nothing else.
#[tokio::test(start_paused = true)]
async fn an_edited_response_leaves_as_its_frozen_image() {
    let stack = admitted_invite("z9hG4bK-img-edited", "img-edited").await;
    let parsed = parse_response(&response_bytes(
        200,
        "OK",
        "INVITE",
        "z9hG4bK-img-edited",
        "img-edited",
        true,
    ));
    let resp = parsed
        .thaw()
        .push_raw(
            HeaderName::from("P-Charging-Vector"),
            "icid-value=icid-9f2c;orig-ioi=bob.example",
        )
        .freeze()
        .expect("an edited 200 is complete");
    assert_ne!(resp.image(), parsed.image(), "the freeze rendered a new image");
    let image = resp.image().to_vec();

    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(TRANSIT_MS * 4).await;

    let wire = raw_from_b2bua(&stack);
    assert_eq!(wire, vec![image], "the wire bytes are the frozen image");
    assert!(
        std::str::from_utf8(&wire[0])
            .unwrap()
            .contains("P-Charging-Vector: icid-value=icid-9f2c;orig-ioi=bob.example\r\n"),
        "the edit is on the wire",
    );
}
