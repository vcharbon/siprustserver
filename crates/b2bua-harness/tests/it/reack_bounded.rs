//! RFC 3261 §13.2.2.4 — the UAC core ACKs each retransmission of a 2xx, and a
//! compliant answerer retransmits that 2xx on the §13.3.1.4 schedule only: at
//! T1, 2·T1, 4·T1, then every T2, giving up at 64·T1. A copy beyond that
//! schedule is not a retransmission the UAC core owes an ACK.
//!
//! The answerer here is broken on purpose: it re-sends its 2xx on every ACK it
//! receives, so each re-ACK draws a new copy at once. The B2BUA re-ACKs at most
//! the copies a compliant answerer can send and sends nothing else, the
//! ping-pong stops, and the call still ends with a BYE, one CDR and is fully
//! reaped. The cases drive the ping-pong on the initial INVITE's 2xx (b-leg) and
//! on a re-INVITE's 2xx in each direction.
//!
//! Each round sends one copy, then reads after a settled advance of a few
//! transits: the verdict depends on no in-order draining of the paused runtime.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, ServerTxn};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE_V2: &str = "v=0\r\no=alice 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_V2: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// One-hop transit: the ping-pong runs at wire speed, far inside 64·T1.
const TRANSIT_MS: u64 = 1;
/// The settled advance after each copy: the copy's hop, the re-ACK's hop, and
/// slack for the SUT's turn.
const ROUND_MS: u64 = 5;
/// The test's own bound on the ping-pong: a SUT that never stops re-ACKing
/// ends the loop here, never the wall clock.
const ROUNDS: usize = 1000;
/// The most copies of one 2xx a compliant answerer sends after the first (RFC
/// 3261 §13.3.1.4, T1 = 500 ms, T2 = 4 s): at 0.5, 1.5, 3.5, 7.5, 11.5, 15.5,
/// 19.5, 23.5, 27.5 and 31.5 s, the next falling past 64·T1 = 32 s.
const COMPLIANT_COPIES: usize = 10;

/// What the SUT put on the wire while the answerer repeated its 2xx.
struct PingPong {
    /// The copies of the 2xx the answerer sent.
    copies: usize,
    /// The ACKs the SUT sent to the answerer.
    re_acks: usize,
    /// The start line of every other message the SUT sent, to anyone.
    others: Vec<String>,
}

impl PingPong {
    /// The SUT re-ACKs the first copy, at most [`COMPLIANT_COPIES`] in all, and
    /// sends nothing else.
    fn assert_bounded(&self) {
        assert!(self.re_acks >= 1, "the SUT re-ACKs the first copy (RFC 3261 §13.2.2.4)");
        assert!(
            self.re_acks <= COMPLIANT_COPIES,
            "the SUT re-ACKs at most the {COMPLIANT_COPIES} copies a compliant answerer sends \
             (RFC 3261 §13.3.1.4), got {} re-ACKs over {} copies",
            self.re_acks,
            self.copies,
        );
        assert!(self.others.is_empty(), "the SUT sends nothing but re-ACKs: {:?}", self.others);
    }
}

/// Plays the broken answerer: re-sends `answer`'s 2xx on `uas` and waits one
/// round, again while anything came back, for at most [`ROUNDS`] copies; then
/// reads what the SUT at `b2bua` sent meanwhile off the recorded wire.
async fn ping_pong(
    h: &Harness,
    b2bua: SocketAddr,
    answerer: &Agent,
    uas: &mut ServerTxn,
    answer: &str,
) -> PingPong {
    let before = h.wire_entries().len();
    let mut copies = 0;
    for _ in 0..ROUNDS {
        uas.respond(200, "OK").with_sdp(answer).await;
        copies += 1;
        h.advance(Duration::from_millis(ROUND_MS)).await;
        if answerer.drain().await == 0 {
            break;
        }
    }
    let (acks, others): (Vec<_>, Vec<_>) = h
        .wire_entries()
        .into_iter()
        .skip(before)
        .filter(|e| e.from == b2bua)
        .partition(|e| e.to == answerer.addr() && e.raw.starts_with(b"ACK "));
    let others = others
        .iter()
        .map(|e| String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string())
        .collect();
    PingPong { copies, re_acks: acks.len(), others }
}

/// The callee answers the initial INVITE and re-sends its 200 on every ACK.
#[tokio::test(start_paused = true)]
async fn a_2xx_repeated_on_every_ack_draws_bounded_re_acks_on_the_b_leg() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-initial", TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    ping_pong(&h, b2bua.addr, &bob, &mut uas, ANSWER).await.assert_bounded();

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}

/// The caller re-INVITEs; the callee answers and re-sends that 200 on every ACK.
#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_repeated_on_every_ack_draws_bounded_re_acks_toward_the_callee() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-reinvite-callee", TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = dialog.reinvite(Some(ALICE_V2)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinvite = bob.receive("INVITE").await;
    bob_reinvite.respond(200, "OK").with_sdp(BOB_V2).await;
    reinvite.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    ping_pong(&h, b2bua.addr, &bob, &mut bob_reinvite, BOB_V2).await.assert_bounded();

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}

/// The callee re-INVITEs; the caller answers and re-sends that 200 on every ACK.
#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_repeated_on_every_ack_draws_bounded_re_acks_toward_the_caller() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-reinvite-caller", TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinvite = bob_dialog.reinvite(Some(BOB_V2)).await;
    let reinvite_cseq = bob_dialog.local_cseq();
    let mut alice_reinvite = alice.receive("INVITE").await;
    alice_reinvite.respond(200, "OK").with_sdp(ALICE_V2).await;
    reinvite.expect(200).await;
    bob_dialog.ack_for(reinvite_cseq, None).await;
    alice.receive("ACK").await;

    ping_pong(&h, b2bua.addr, &alice, &mut alice_reinvite, ALICE_V2).await.assert_bounded();

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}
