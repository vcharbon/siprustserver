//! An in-dialog request relayed to the other leg whose copy toward that leg is
//! past the largest UDP payload, though the request itself fit: its sender is
//! answered 513 Message Too Large at once (RFC 3261 §21.5.14) and the session
//! stands. A re-INVITE refused this way leaves the session unchanged (§14.1);
//! a non-INVITE draws no 408 (RFC 4320 §4.2). The other party hears nothing.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::B2buaScene;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

/// The largest UDP payload over IPv4.
const DATAGRAM: usize = 65_507;
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// SDP attribute lines adding `extra` bytes.
fn pad_lines(extra: usize) -> String {
    let mut out = String::new();
    let mut left = extra;
    while left >= 1_007 {
        out.push_str(&format!("a=x:{}\r\n", "f".repeat(1_000 - 6)));
        left -= 1_000;
    }
    if left > 0 {
        assert!(left >= 7, "{left}");
        out.push_str(&format!("a=x:{}\r\n", "f".repeat(left - 6)));
    }
    out
}

/// The datagrams sent from `from` to `to` whose start line opens with `start`,
/// by length.
fn sent(h: &Harness, from: SocketAddr, to: SocketAddr, start: &str) -> Vec<usize> {
    let report = h.snapshot_report();
    let entries = report.entries();
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(start.as_bytes()))
        .map(|e| e.raw.len())
        .collect()
}

/// The bytes by which a request of `start` grows from alice's datagram to the
/// worker's copy toward bob, read off the last one relayed.
fn growth(s: &B2buaScene, start: &str) -> (usize, usize) {
    let to_b2bua = sent(&s.h, s.alice.addr(), s.b2bua.addr, start);
    let to_bob = sent(&s.h, s.b2bua.addr, s.bob.addr(), start);
    let (alice, worker) = (*to_b2bua.last().expect("sent"), *to_bob.last().expect("relayed"));
    assert!(worker > alice + 4, "the copy toward bob is longer: {alice} {worker}");
    (alice, worker - alice)
}

/// Alice's re-INVITE fits a datagram, its copy toward bob does not: she gets
/// 513, bob gets nothing, and the call goes on to her BYE with its CDR.
#[tokio::test(start_paused = true)]
async fn a_relayed_reinvite_that_cannot_leave_draws_513_and_keeps_the_call() {
    let s = B2buaScene::new("relayed-reinvite-too-large").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut fits = dialog.reinvite(Some(REOFFER)).await;
    let cseq = dialog.local_cseq();
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(REANSWER).await;
    fits.expect(200).await;
    dialog.ack_for(cseq, None).await;
    s.bob.receive("ACK").await;
    let (alice, grows) = growth(&s, "INVITE ");
    let relayed = sent(&s.h, s.b2bua.addr, s.bob.addr(), "INVITE ").len();

    // Its Content-Length gains two digits.
    let padded = DATAGRAM - alice - grows / 2 - 2;
    let big = format!("{}{}", REOFFER.replace("o=alice 1 2", "o=alice 1 3"), pad_lines(padded));
    let started = tokio::time::Instant::now();
    let mut reinvite = dialog.reinvite(Some(&big)).await;
    reinvite.expect(513).await;
    assert!(started.elapsed() < Duration::from_secs(1), "at once: {:?}", started.elapsed());
    let to_bob = sent(&s.h, s.b2bua.addr, s.bob.addr(), "INVITE ").len();
    assert_eq!(to_bob, relayed, "bob gets no re-INVITE");

    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(s.b2bua.active_calls(), 1, "the call stands");
    s.hangup(&mut dialog).await;
    s.h.advance(Duration::from_millis(100)).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call writes its CDR");
    s.finish().await;
}

/// Alice's `method` with a body that fits a datagram, whose copy toward bob does
/// not: she gets 513 at once, bob gets nothing, and the call goes on to her BYE
/// with its CDR.
async fn a_relayed_request_that_cannot_leave(method: InDialogMethod, start: &str) {
    let s = B2buaScene::new("relayed-request-too-large").await;
    let mut dialog = s.establish().await;

    let mut fits = dialog.send_request(method).with_body("text/plain", b"x".to_vec()).send().await;
    s.bob.receive(start.trim_end()).await.respond(200, "OK").await;
    fits.expect(200).await;
    let (alice, grows) = growth(&s, start);
    let relayed = sent(&s.h, s.b2bua.addr, s.bob.addr(), start).len();

    // The body replaces one byte, and its Content-Length gains four digits.
    let body = "y".repeat(DATAGRAM - alice - grows / 2 - 3);
    let started = tokio::time::Instant::now();
    let mut big =
        dialog.send_request(method).with_body("text/plain", body.into_bytes()).send().await;
    big.expect(513).await;
    assert!(started.elapsed() < Duration::from_secs(1), "at once: {:?}", started.elapsed());
    assert_eq!(sent(&s.h, s.b2bua.addr, s.bob.addr(), start).len(), relayed, "bob gets nothing");

    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(s.b2bua.active_calls(), 1, "the call stands");
    s.hangup(&mut dialog).await;
    s.h.advance(Duration::from_millis(100)).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call writes its CDR");
    s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_relayed_info_that_cannot_leave_draws_513_and_keeps_the_call() {
    a_relayed_request_that_cannot_leave(InDialogMethod::Info, "INFO ").await;
}

#[tokio::test(start_paused = true)]
async fn a_relayed_message_that_cannot_leave_draws_513_and_keeps_the_call() {
    a_relayed_request_that_cannot_leave(InDialogMethod::Message, "MESSAGE ").await;
}
