//! ADR-0063 §1 for the copies of the caller's initial INVITE once it is
//! answered: the caller, broken on purpose, ACKs the 2xx and keeps re-sending
//! that INVITE every 2 s past 64·T1. The Accepted transaction absorbs the
//! copies (RFC 6026 §7.1), and a copy after Timer L ended it opens no
//! transaction and draws no 482 (RFC 3261 §8.2.2.2): the INVITE's count
//! outlives its transaction, and no copy later than 64·T1 after the first is
//! answered. A lone copy after the record lapsed opens a transaction and is
//! absorbed as a retransmission (§17.2.3). The call then ends with the
//! caller's BYE: one CDR, fully reaped.
//!
//! Each copy is read after a settled advance of a few transits: the verdict
//! depends on no in-order draining of the paused runtime.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// 64·T1 (RFC 3261 §17.1.1.1, T1 = 500 ms): Timer B, H and L.
const WINDOW_MS: u64 = 64 * 500;
/// The caller's pace: past T1, so each copy would be owed an answer were it
/// owed one at all.
const EVERY_MS: u64 = 2_000;

/// The caller's initial INVITE on the recorded wire.
fn first_invite(h: &Harness, from: SocketAddr, to: SocketAddr) -> Vec<u8> {
    h.wire_entries()
        .into_iter()
        .find(|e| e.from == from && e.to == to && e.raw.starts_with(b"INVITE "))
        .map(|e| e.raw.to_vec())
        .expect("the caller's INVITE")
}

/// The status lines of the INVITE responses the SUT at `b2bua` sent `to` in
/// the wire entries past `from`.
fn invite_answers(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> Vec<String> {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to && e.raw.starts_with(b"SIP/2.0 "))
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .filter(|text| text.contains(" INVITE\r\n"))
        .map(|text| text.lines().next().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_callers_invite_re_sent_past_64_t1_after_its_2xx_draws_nothing() {
    let h = Harness::with_transit_delay("b2bua-late-invite-copies", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let invite = first_invite(&h, alice.addr(), b2bua.addr);

    let before = h.wire_entries().len();
    for _ in 0..(WINDOW_MS + 10_000) / EVERY_MS {
        alice.try_send_datagram(&invite, b2bua.addr).await.expect("the copy leaves");
        h.advance(Duration::from_millis(EVERY_MS)).await;
        alice.drain().await;
    }
    let answered = invite_answers(&h, b2bua.addr, alice.addr(), before);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    assert!(answered.is_empty(), "no copy of the answered INVITE draws an answer: {answered:?}");
    h.finish().await;
}

/// One copy of the caller's answered INVITE, on its own branch, after Timer L
/// ended its transaction and 64·T1 of silence lapsed its copy record: a
/// retransmission of a request already answered (RFC 3261 §17.2.3), not a
/// merged request (§8.2.2.2). It draws nothing, and the call stays up until
/// the caller's BYE: one CDR, fully reaped.
#[tokio::test(start_paused = true)]
async fn one_copy_of_the_answered_invite_after_its_transaction_and_record_ended_draws_nothing() {
    let h = Harness::with_transit_delay("b2bua-lone-late-invite-copy", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let invite = first_invite(&h, alice.addr(), b2bua.addr);

    h.advance(Duration::from_millis(WINDOW_MS + 10_000)).await;
    let before = h.wire_entries().len();
    alice.try_send_datagram(&invite, b2bua.addr).await.expect("the copy leaves");
    h.advance(Duration::from_millis(EVERY_MS)).await;
    alice.drain().await;
    let answered = invite_answers(&h, b2bua.addr, alice.addr(), before);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    assert!(answered.is_empty(), "the lone late copy draws no answer: {answered:?}");
    h.finish().await;
}
