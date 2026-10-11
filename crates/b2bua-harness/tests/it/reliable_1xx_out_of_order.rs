//! **RFC 3262 §4 — a reliable provisional out of its dialog's `RSeq` order is
//! neither PRACKed nor processed further.** Its `RSeq` must be one higher than
//! the last reliable provisional the UAC took on that early dialog; one that
//! skips ahead is dropped where it arrives, whatever a policy would have made
//! of it: no PRACK toward the responder, nothing relayed toward the caller.
//!
//! The shapes are the policies that hide the callee's reliable provisionals
//! from the caller and acknowledge them on this stack's own account: the
//! 18x mask relaying every 18x as a bare 180, and the promotion of a 183 with
//! early media to a 200.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::features::{Relay18xMessages, RelayFirst18xStrategy};
use scenario_harness::{Harness, WaiverScope};
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The callee skips an `RSeq`: the non-compliant provisional whose handling
/// is the subject. Dropped unPRACKed, it is still outstanding when bob
/// answers: the 2xx he sends over it is his own fixture's consequence.
fn callee_skips_an_rseq(h: &Harness) {
    h.waive(
        WaiverScope::rule(
            "non-contiguous-rseq",
            "bob skips an RSeq — the provisional this stack must drop unprocessed is the subject",
        )
        .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "bob answers over his own out-of-order provisional, which §4 leaves unPRACKed",
        )
        .on_party("bob"),
    );
}

/// The PRACKs the SUT put on `to`'s wire, first copies only, as their `RAck`.
fn pracks_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"PRACK "))
        .filter(|e| e.reemit.is_none())
        .filter_map(|e| {
            let text = std::str::from_utf8(&e.raw).ok()?;
            text.split("\r\n")
                .find(|l| l.len() > 5 && l[..5].eq_ignore_ascii_case("RAck:"))
                .map(|l| l[5..].trim().to_string())
        })
        .collect()
}

/// The mask relays every 18x as a bare 180 and PRACKs the callee itself. The
/// callee's reliable 183 `RSeq 1` is shown and PRACKed; its next reliable 183
/// states `RSeq 3`: the caller sees no second 180 and the callee no second
/// PRACK.
#[tokio::test(start_paused = true)]
async fn the_mask_drops_an_out_of_order_reliable_1xx() {
    let h = Harness::with_transit_delay("b2bua-reliable-1xx-out-of-order-mask", 0);
    callee_skips_an_rseq(&h);
    let alice = h.agent("alice", "127.0.0.1:7511").await;
    let bob = h.agent("bob", "127.0.0.1:7512").await;
    let b2bua = B2buaSut::route_all_to_with_18x_messages(
        "127.0.0.1",
        7512,
        RelayFirst18xStrategy::DropSdp,
        Relay18xMessages::All,
    )
    .start(&h, "b2bua", "127.0.0.1:7513")
    .await;
    let bob_addr: SocketAddr = "127.0.0.1:7512".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();

    uas.respond(183, "Session Progress").reliable(1).with_sdp(EARLY).await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    uas.respond(183, "Session Progress").reliable(3).with_sdp(EARLY).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "a reliable provisional out of RSeq order is not PRACKed (RFC 3262 §4)",
    );

    uas.respond(200, "OK").with_sdp(EARLY).await;
    let answer = call.expect(200).await;
    assert_eq!(answer.status(), 200, "the next response the caller sees is the answer");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr),
        vec![format!("1 {invite_cseq} INVITE")],
        "one PRACK, for the in-order provisional alone",
    );
}

/// The caller is shown the callee's reliable 180 `RSeq 1` and PRACKs it end to
/// end. The callee's next reliable provisional is a 183 with early media that
/// states `RSeq 3`: it is not promoted to a 200 toward the caller, and the
/// callee is not PRACKed for it.
#[tokio::test(start_paused = true)]
async fn the_promotion_drops_an_out_of_order_reliable_183() {
    let h = Harness::with_transit_delay("b2bua-reliable-1xx-out-of-order-promote", 0);
    callee_skips_an_rseq(&h);
    let alice = h.agent("alice", "127.0.0.1:7514").await;
    let bob = h.agent("bob", "127.0.0.1:7515").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 7515, RelayFirst18xStrategy::PromotePemTo200)
            .start(&h, "b2bua", "127.0.0.1:7516")
            .await;
    let bob_addr: SocketAddr = "127.0.0.1:7515".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();

    uas.respond(180, "Ringing").reliable(1).with_sdp(EARLY).await;
    let ringing = call.expect(180).await;
    let mut prack = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(183, "Session Progress")
        .reliable(3)
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "a reliable provisional out of RSeq order is not PRACKed (RFC 3262 §4)",
    );

    uas.respond(200, "OK").with_sdp(EARLY).await;
    let answer = call.expect(200).await;
    assert_eq!(
        answer.to().tag(),
        ringing.to().tag(),
        "the caller's 200 is the callee's answer on the dialog she rang on, not a promotion",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr),
        vec![format!("1 {invite_cseq} INVITE")],
        "one PRACK, the caller's relayed one, for the in-order provisional alone",
    );
}
