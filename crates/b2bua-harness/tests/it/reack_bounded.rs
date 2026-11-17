//! RFC 3261 §13.2.2.4 — the UAC core ACKs each retransmission of a 2xx, and a
//! compliant answerer retransmits that 2xx on the §13.3.1.4 schedule only: at
//! T1, 2·T1, 4·T1, then every T2, giving up at 64·T1. A copy beyond that
//! schedule's count, arriving within T1 of the last re-ACK, is not a
//! retransmission the UAC core owes an ACK.
//!
//! The ping-pong answerer is broken on purpose: it re-sends its 2xx on every
//! ACK it receives, so each re-ACK draws a new copy at once. The B2BUA re-ACKs
//! the copies a compliant answerer can send and sends nothing else, the
//! ping-pong stops, and the call still ends with a BYE, one CDR and is fully
//! reaped. The cases drive the ping-pong on the initial INVITE's 2xx (b-leg),
//! on a re-INVITE's 2xx in each direction, on a fork straggler's 2xx, and on
//! the initial INVITE's 2xx while a re-INVITE is pending; a copy of a 2xx two
//! INVITEs back finds no ACK to repeat and is absorbed.
//!
//! The spaced answerers never see an ACK (each is lost): one re-sends on this
//! stack's own schedule, then past the count just within and at T1 of the last
//! re-ACK; one on the schedule of a larger T1 (§17.1.1.1) past the count. Every
//! copy on a schedule draws its re-ACK, and past the count only one T1 apart.
//!
//! Each round sends one copy, then reads after a settled advance of a few
//! transits: the verdict depends on no in-order draining of the paused runtime.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua::metrics::B2buaMetrics;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, ServerTxn};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE_V2: &str = "v=0\r\no=alice 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_V2: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_V3: &str = "v=0\r\no=alice 1 3 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
const BOB_V3: &str = "v=0\r\no=bob 1 3 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20006 RTP/AVP 0\r\n";
const STRAGGLER_ANSWER: &str = "v=0\r\no=bob 2 1 IN IP4 192.0.2.21\r\ns=-\r\nc=IN IP4 192.0.2.21\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";

/// The To-tag of the fork that wins the call.
const WINNER: &str = "fork-1";
/// The To-tag of the fork whose 2xx comes after the winner's.
const STRAGGLER: &str = "fork-2";

/// One-hop transit: the ping-pong runs at wire speed, far inside 64·T1.
const TRANSIT_MS: u64 = 1;
/// The settled advance after each copy: the copy's hop, the re-ACK's hop, and
/// slack for the SUT's turn.
const ROUND_MS: u64 = 5;
/// This stack's T1 (RFC 3261 §17.1.1.1).
const T1: Duration = Duration::from_millis(500);
/// The step just short of a threshold.
const ONE_MS: Duration = Duration::from_millis(1);
/// The test's own bound on the ping-pong: a SUT that never stops re-ACKing
/// ends the loop here, never the wall clock.
const ROUNDS: usize = 1000;
/// The most copies of one 2xx a compliant answerer sends after the first (RFC
/// 3261 §13.3.1.4, T1 = 500 ms, T2 = 4 s): at 0.5, 1.5, 3.5, 7.5, 11.5, 15.5,
/// 19.5, 23.5, 27.5 and 31.5 s, the next falling past 64·T1 = 32 s.
const COMPLIANT_COPIES: usize = 10;
/// When a compliant answerer with this stack's T1 = 500 ms and T2 = 4 s
/// re-sends its 2xx, in ms after the first send (§13.3.1.4).
const OWN_SCHEDULE_MS: [u64; COMPLIANT_COPIES] =
    [500, 1_500, 3_500, 7_500, 11_500, 15_500, 19_500, 23_500, 27_500, 31_500];
/// When a compliant answerer with T1 = 1 s and T2 = 4 s re-sends its 2xx, in
/// ms after the first send, up to the first copy whose ACK gets through: the
/// path drops every ACK for 40 s.
const LARGER_T1_SCHEDULE_MS: [u64; 12] =
    [1_000, 3_000, 7_000, 11_000, 15_000, 19_000, 23_000, 27_000, 31_000, 35_000, 39_000, 43_000];

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
    /// The SUT re-ACKs the [`COMPLIANT_COPIES`] copies a compliant answerer
    /// can send whatever their pace, absorbs the next one, which arrives within
    /// T1 of the last re-ACK, and sends nothing else; each re-ACK and the
    /// absorbed copy are quiet turns, and `metrics` counts the copy absorbed.
    fn assert_bounded(&self, metrics: &B2buaMetrics) {
        assert_eq!(
            self.re_acks, COMPLIANT_COPIES,
            "the SUT re-ACKs the {COMPLIANT_COPIES} copies a compliant answerer sends (RFC 3261 \
             §13.3.1.4), got {} re-ACKs over {} copies",
            self.re_acks, self.copies,
        );
        assert_eq!(self.copies, COMPLIANT_COPIES + 1, "the first copy left un-ACKed ends it");
        assert!(self.others.is_empty(), "the SUT sends nothing but re-ACKs: {:?}", self.others);
        assert_eq!(
            metrics.copies_past_schedule_total("2xx"),
            1,
            "the copy left un-ACKed is counted"
        );
        assert_eq!(
            metrics.repl_quiet_turns_total("re-ack"),
            u64::try_from(COMPLIANT_COPIES).expect("a count"),
            "each re-ACK is a quiet turn",
        );
        assert_eq!(
            metrics.repl_quiet_turns_total("absorbed-copy"),
            1,
            "the absorbed copy is a quiet turn",
        );
    }
}

/// Sends one copy of the 2xx `uas` answers with, carrying `answer` and, for a
/// fork other than the transaction's own, `to_tag`.
async fn send_copy(uas: &mut ServerTxn, answer: &str, to_tag: Option<&str>) {
    let reply = uas.respond(200, "OK").with_sdp(answer);
    match to_tag {
        Some(tag) => reply.with_to_tag(tag).await,
        None => reply.await,
    }
}

/// The ACKs the SUT at `b2bua` sent to `to` in the wire entries past `from`.
fn acks_to(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> usize {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to && e.raw.starts_with(b"ACK "))
        .count()
}

/// Plays the broken answerer: re-sends `answer`'s 2xx on `uas` (under
/// `to_tag`, when given) and waits one round, again while anything came back,
/// for at most [`ROUNDS`] copies; then reads what the SUT at `b2bua` sent
/// meanwhile off the recorded wire.
async fn ping_pong(
    h: &Harness,
    b2bua: SocketAddr,
    answerer: &Agent,
    uas: &mut ServerTxn,
    (answer, to_tag): (&str, Option<&str>),
) -> PingPong {
    let before = h.wire_entries().len();
    let mut copies = 0;
    for _ in 0..ROUNDS {
        send_copy(uas, answer, to_tag).await;
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

/// Plays an answerer that never sees an ACK: re-sends `uas`'s 2xx with
/// `answer` at `due` and returns the ACKs the SUT at `b2bua` sent it for that
/// copy.
async fn copy_at(
    h: &Harness,
    b2bua: SocketAddr,
    answerer: &Agent,
    uas: &mut ServerTxn,
    (answer, due): (&str, tokio::time::Instant),
) -> usize {
    h.advance(due.saturating_duration_since(tokio::time::Instant::now())).await;
    let before = h.wire_entries().len();
    send_copy(uas, answer, None).await;
    h.advance(Duration::from_millis(ROUND_MS)).await;
    answerer.drain().await;
    acks_to(h, b2bua, answerer.addr(), before)
}

/// Plays a spaced answerer that never sees an ACK: re-sends `uas`'s 2xx with
/// `answer` at each offset of `schedule_ms` after `sent` and asserts that each
/// copy draws exactly one re-ACK from the SUT at `b2bua` and that the SUT
/// sends nothing else meanwhile.
async fn spaced_copies(
    h: &Harness,
    b2bua: SocketAddr,
    answerer: &Agent,
    uas: &mut ServerTxn,
    (answer, sent): (&str, tokio::time::Instant),
    schedule_ms: &[u64],
) {
    let start = h.wire_entries().len();
    for (n, offset) in schedule_ms.iter().enumerate() {
        let due = sent + Duration::from_millis(*offset);
        assert_eq!(
            copy_at(h, b2bua, answerer, uas, (answer, due)).await,
            1,
            "copy {} at {offset} ms draws its re-ACK (RFC 3261 §13.2.2.4)",
            n + 1,
        );
    }
    let sent_by_sut = h.wire_entries().into_iter().skip(start).filter(|e| e.from == b2bua).count();
    assert_eq!(sent_by_sut, schedule_ms.len(), "the SUT sends nothing but the re-ACKs");
}

/// Ends the call from alice's side and checks it is fully reaped with one CDR.
async fn hang_up(h: Harness, b2bua: &B2buaSut, bob: &Agent, dialog: &mut scenario_harness::Dialog) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
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

    ping_pong(&h, b2bua.addr, &bob, &mut uas, (ANSWER, None)).await.assert_bounded(b2bua.metrics());

    hang_up(h, &b2bua, &bob, &mut dialog).await;
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

    ping_pong(&h, b2bua.addr, &bob, &mut bob_reinvite, (BOB_V2, None))
        .await
        .assert_bounded(b2bua.metrics());

    hang_up(h, &b2bua, &bob, &mut dialog).await;
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

    ping_pong(&h, b2bua.addr, &alice, &mut alice_reinvite, (ALICE_V2, None))
        .await
        .assert_bounded(b2bua.metrics());

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}

/// The callee re-sends the 200 to the initial INVITE once, then the caller
/// re-INVITEs and, while the callee's answer is pending, the callee re-sends
/// that first 200 on every ACK. Each copy answers the earlier INVITE (RFC 3261
/// §12.2.1.2), not the pending one: it is re-ACKed with the ACK of that 200,
/// on the count the copy before the re-INVITE started, and the re-INVITE then
/// completes.
#[tokio::test(start_paused = true)]
async fn an_earlier_2xx_repeated_on_every_ack_while_a_reinvite_is_pending_draws_bounded_re_acks() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-earlier-2xx", TRANSIT_MS);
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
    let before = h.wire_entries().len();
    send_copy(&mut uas, ANSWER, None).await;
    h.advance(Duration::from_millis(ROUND_MS)).await;
    bob.drain().await;
    assert_eq!(acks_to(&h, b2bua.addr, bob.addr(), before), 1, "the first copy draws its re-ACK");

    let mut reinvite = dialog.reinvite(Some(ALICE_V2)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinvite = bob.receive("INVITE").await;

    let pong = ping_pong(&h, b2bua.addr, &bob, &mut uas, (ANSWER, None)).await;
    assert_eq!(
        pong.re_acks,
        COMPLIANT_COPIES - 1,
        "the copies before and during the re-INVITE share one count (ADR-0057 §10)"
    );
    assert_eq!(pong.copies, COMPLIANT_COPIES, "the first copy left un-ACKed ends it");
    assert!(pong.others.is_empty(), "the SUT sends nothing but re-ACKs: {:?}", pong.others);
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        1,
        "the absorbed copy is counted"
    );

    bob_reinvite.respond(200, "OK").with_sdp(BOB_V2).await;
    reinvite.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}

/// Two re-INVITEs after the initial INVITE, a copy of the 200 to that INVITE
/// finds no ACK of it on the record: it answers nothing pending, draws nothing
/// and is counted absorbed.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_2xx_two_invites_back_is_absorbed() {
    let h = Harness::with_transit_delay("b2bua-reack-two-invites-back", TRANSIT_MS);
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
    for (offer, answer) in [(ALICE_V2, BOB_V2), (ALICE_V3, BOB_V3)] {
        let mut reinvite = dialog.reinvite(Some(offer)).await;
        let reinvite_cseq = dialog.local_cseq();
        bob.receive("INVITE").await.respond(200, "OK").with_sdp(answer).await;
        reinvite.expect(200).await;
        dialog.ack_for(reinvite_cseq, None).await;
        bob.receive("ACK").await;
    }

    let before = h.wire_entries().len();
    send_copy(&mut uas, ANSWER, None).await;
    h.advance(Duration::from_millis(ROUND_MS)).await;
    bob.drain().await;
    let sent = h.wire_entries().into_iter().skip(before).filter(|e| e.from == b2bua.addr).count();
    assert_eq!(sent, 0, "the copy answers nothing pending and draws nothing");
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        1,
        "the absorbed copy is counted"
    );
    assert_eq!(b2bua.metrics().repl_quiet_turns_total("absorbed-copy"), 1, "a quiet turn");

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}

/// A fork answers and wins; a second fork's 2xx follows on the same INVITE,
/// is ACKed and BYEd (RFC 3261 §13.2.2.4), then re-sent on every ACK.
#[tokio::test(start_paused = true)]
async fn a_fork_stragglers_2xx_repeated_on_every_ack_draws_bounded_re_acks() {
    let h = Harness::with_transit_delay("b2bua-reack-bounded-straggler", TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag(WINNER).await;
    call.expect(180).await;
    uas.adopt_to_tag(WINNER);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    send_copy(&mut uas, STRAGGLER_ANSWER, Some(STRAGGLER)).await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    ping_pong(&h, b2bua.addr, &bob, &mut uas, (STRAGGLER_ANSWER, Some(STRAGGLER)))
        .await
        .assert_bounded(b2bua.metrics());

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}

/// The callee re-sends its 200 on this stack's own schedule, every ACK lost:
/// each of the ten copies draws its re-ACK; an eleventh, at ping-pong pace
/// right after the tenth, draws none and is counted; a twelfth T1 − 1 ms after
/// the tenth draws none either, a thirteenth T1 after it draws one (§17.1.1.1).
#[tokio::test(start_paused = true)]
async fn a_2xx_resent_on_the_rfc_schedule_draws_a_re_ack_per_copy() {
    let h = Harness::with_transit_delay("b2bua-reack-own-schedule", TRANSIT_MS);
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
    let sent = tokio::time::Instant::now();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    spaced_copies(&h, b2bua.addr, &bob, &mut uas, (ANSWER, sent), &OWN_SCHEDULE_MS).await;
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        0,
        "no copy is past the schedule"
    );

    let before = h.wire_entries().len();
    send_copy(&mut uas, ANSWER, None).await;
    h.advance(Duration::from_millis(ROUND_MS)).await;
    bob.drain().await;
    assert_eq!(acks_to(&h, b2bua.addr, bob.addr(), before), 0, "the eleventh copy is absorbed");
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        1,
        "the absorbed copy is counted"
    );

    let tenth = sent + Duration::from_millis(OWN_SCHEDULE_MS[COMPLIANT_COPIES - 1]);
    let early = copy_at(&h, b2bua.addr, &bob, &mut uas, (ANSWER, tenth + T1 - ONE_MS)).await;
    assert_eq!(early, 0, "a copy past the count within T1 of the last re-ACK is absorbed");
    assert_eq!(b2bua.metrics().copies_past_schedule_total("2xx"), 2, "the early copy is counted");
    let spaced = copy_at(&h, b2bua.addr, &bob, &mut uas, (ANSWER, tenth + T1)).await;
    assert_eq!(spaced, 1, "a copy past the count T1 after the last re-ACK draws its re-ACK");
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        2,
        "the re-ACKed copy is not counted"
    );

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}

/// The callee runs a larger T1 (RFC 3261 §17.1.1.1: T1 = 1 s, T2 = 4 s) and
/// the path drops every ACK for 40 s: its copies go past this stack's count,
/// each at least T1 after the last re-ACK, so each draws its re-ACK; the
/// twelfth's gets through and the call stays up.
#[tokio::test(start_paused = true)]
async fn a_larger_t1_answerers_copies_past_the_count_draw_their_re_acks() {
    let h = Harness::with_transit_delay("b2bua-reack-larger-t1", TRANSIT_MS);
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
    let sent = tokio::time::Instant::now();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    spaced_copies(&h, b2bua.addr, &bob, &mut uas, (ANSWER, sent), &LARGER_T1_SCHEDULE_MS).await;
    assert_eq!(b2bua.metrics().copies_past_schedule_total("2xx"), 0, "no copy is absorbed");
    assert_eq!(b2bua.active_calls(), 1, "the call is still up");

    hang_up(h, &b2bua, &bob, &mut dialog).await;
}
