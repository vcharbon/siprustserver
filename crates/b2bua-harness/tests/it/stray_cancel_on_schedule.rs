//! ADR-0063 §1 for the 481 a CANCEL matching no INVITE draws (RFC 3261
//! §9.2): the caller's call was answered and ended, its INVITE's Accepted
//! transaction gone at Timer L, and the caller, broken on purpose, then
//! CANCELs that INVITE and re-sends the CANCEL on every 481. The CANCEL is
//! answered through its own non-INVITE transaction: the 481 leaves for it and
//! the copies Timer E re-sends, at any pace, and the rest are absorbed. The
//! call itself ended with one CDR, fully reaped.
//!
//! Each step reads after a settled advance of a few transits: the verdict
//! depends on no in-order draining of the paused runtime.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// 64·T1 (RFC 3261 §17.1.1.1): Timer L.
const WINDOW_MS: u64 = 64 * 500;
/// The copies of a CANCEL a compliant client re-sends on Timer E (§17.1.2.2,
/// T1 = 500 ms, T2 = 4 s) before Timer F = 64·T1: at 0.5, 1.5, 3.5, 7.5, 11.5,
/// 15.5, 19.5, 23.5, 27.5 and 31.5 s.
const CANCEL_RESENDS: usize = 10;
/// The copies of the burst, past Timer E's re-sends.
const COPIES: usize = 15;

/// The CANCEL of `invite`, the caller's INVITE as it left (§9.1): its
/// Request-URI, Call-ID, To, From, CSeq number and top Via, no body.
fn cancel_of(invite: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(invite).to_string();
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    let mut lines = Vec::new();
    for (i, line) in head.split("\r\n").enumerate() {
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            _ if i == 0 => lines.push(line.replacen("INVITE ", "CANCEL ", 1)),
            "cseq" => lines.push(format!("{name}:{}", value.replace("INVITE", "CANCEL"))),
            "content-type" | "content-length" | "contact" => {}
            _ => lines.push(line.to_string()),
        }
    }
    lines.push("Content-Length: 0".into());
    format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes()
}

/// The caller's initial INVITE on the recorded wire.
fn first_invite(h: &Harness, from: SocketAddr, to: SocketAddr) -> Vec<u8> {
    h.wire_entries()
        .into_iter()
        .find(|e| e.from == from && e.to == to && e.raw.starts_with(b"INVITE "))
        .map(|e| e.raw.to_vec())
        .expect("the caller's INVITE")
}

/// The 481s the SUT at `b2bua` sent `to` in the wire entries past `from`.
fn gone_to(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> usize {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to && e.raw.starts_with(b"SIP/2.0 481 "))
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_stray_cancel_re_sent_on_every_481_draws_it_on_timer_es_schedule() {
    let h = Harness::with_transit_delay("b2bua-stray-cancel-481", 1);
    h.waive(
        WaiverScope::rule(
            "no-cancel-after-final",
            "alice CANCELs her answered INVITE after the call ended (RFC 3261 §9.1), \
             the stray CANCEL whose answers this test bounds",
        )
        .on_party("alice"),
    );
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
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.advance(Duration::from_millis(WINDOW_MS + 1_000)).await;

    let cancel = cancel_of(&first_invite(&h, alice.addr(), b2bua.addr));
    let before = h.wire_entries().len();
    for _ in 0..=COPIES {
        alice.try_send_datagram(&cancel, b2bua.addr).await.expect("the CANCEL leaves");
        h.advance(Duration::from_millis(1)).await;
    }
    h.advance(Duration::from_millis(20)).await;
    alice.drain().await;
    let answered = gone_to(&h, b2bua.addr, alice.addr(), before);
    h.finish().await;
    assert_eq!(answered, 1 + CANCEL_RESENDS, "the 481 and Timer E's re-sends, the rest absorbed");
}
