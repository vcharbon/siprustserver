//! **PARANOID** — a straggling fork's 2xx leaves nothing behind on our side.
//!
//! RFC 3261 §13.2.2.4: "Multiple 2xx responses may arrive at the UAC for a
//! single INVITE request due to a forking proxy", each distinguished by its
//! To-tag and each a distinct dialog. A single UAS never sends two: §13.3.1.4
//! has it generate ONE 2xx and retransmit THAT response until ACKed. So the
//! straggler here is a second remote UA behind the fork (§13: "multiple 2xx
//! responses ... received from different remote UAs (because the INVITE
//! forked)"), scripted on bob's socket because the wire is all the SUT sees.
//!
//! The B2BUA confirms the winner and releases the straggler as §13.2.2.4 has a
//! UAC do with a dialog it does not want: it ACKs that 2xx, then BYEs that
//! dialog (`release-fork-straggler-2xx`). What this cell asserts is that the
//! release costs our call nothing: the caller keeps the one answer she was
//! given, the hangup BYE addresses the WINNING tag, the CDR is written, and
//! the call record reaps empty.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER_WINNER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER_STRAGGLER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

const ALICE: &str = "127.0.0.1:6061";
const BOB: &str = "127.0.0.1:6062";
const B2BUA: &str = "127.0.0.1:6063";

const WINNER: &str = "bobfork1";
const STRAGGLER: &str = "bobfork2";

/// Two forks ring, the first answers and the call bridges; the second answers
/// afterwards on the same INVITE CSeq under its own tag. The B2BUA ACKs and BYEs
/// the straggler, and its own call stays exactly as it was — one answer to
/// alice, a hangup BYE on the winner's tag, a CDR, and a reaped record.
#[tokio::test(start_paused = true)]
async fn a_fork_straggler_leaves_our_call_intact_and_reaped() {
    let h = Harness::new("b2bua-fork-straggler-no-leak");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6062).start(&h, "b2bua", B2BUA).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // ── two forks ring, so the b-leg holds two early dialogs (§12.1.2) ───────
    uas.respond(180, "Ringing").with_to_tag(WINNER).await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag(STRAGGLER).await;
    call.expect(180).await;

    // ── the first fork answers: the b-leg confirms under its tag ─────────────
    uas.adopt_to_tag(WINNER);
    uas.respond(200, "OK").with_sdp(ANSWER_WINNER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let winner_ack = bob.receive("ACK").await;
    assert_eq!(
        winner_ack.request().to().tag(),
        Some(WINNER),
        "the relayed ACK addresses the dialog that answered",
    );

    // ── the second fork answers late, same INVITE CSeq, its own tag ──────────
    uas.respond(200, "OK").with_to_tag(STRAGGLER).with_sdp(ANSWER_STRAGGLER).await;
    // §13.2.2.4: the UAC ACKs every 2xx, then BYEs the dialog it does not want.
    let straggler_ack = bob.receive("ACK").await;
    assert_eq!(straggler_ack.request().to().tag(), Some(STRAGGLER), "the ACK names the straggler");
    let mut straggler_bye = bob.receive("BYE").await;
    assert_eq!(straggler_bye.request().to().tag(), Some(STRAGGLER), "the BYE ends the straggler");
    straggler_bye.respond(200, "OK").await;
    for _ in 0..5 {
        h.advance(Duration::from_millis(100)).await;
    }
    bob.drain().await;
    assert_eq!(
        datagrams_to_bob_tagged(&h, &b2bua, STRAGGLER),
        2,
        "the straggler's dialog draws its ACK and its BYE, once each",
    );
    assert_eq!(
        alice_finals(&h, &b2bua),
        1,
        "alice keeps the one 200 OK she was given for her INVITE",
    );

    // ── alice hangs up: our b-leg BYE rides the WINNER's tag ─────────────────
    let mut bye = dialog.bye().await;
    let mut b_bye = bob.receive("BYE").await;
    assert_eq!(
        b_bye.request().to().tag(),
        Some(WINNER),
        "the hangup BYE terminates the dialog this call confirmed",
    );
    b_bye.respond(200, "OK").await;
    bye.expect(200).await;

    // ── nothing is left on the platform ──────────────────────────────────────
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.active_calls(), 0, "no call record survives the hangup");
    b2bua.assert_fully_reaped();

    // ── and the call is accounted for ────────────────────────────────────────
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the one call: {cdrs:?}");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Answer), "the CDR records the answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "the CDR records the hangup: {kinds:?}");

    assert_eq!(
        datagrams_to_bob_tagged(&h, &b2bua, STRAGGLER),
        2,
        "the whole call through, the straggler's dialog drew nothing more",
    );

    alice.drain().await;
    bob.drain().await;
    let _report = h.finish().await;
}

/// A straggler that repeats its 2xx (RFC 3261 §13.3.1.4: its ACK was lost, as
/// far as it knows) draws the SAME ACK again — one ACK per 2xx, re-passed to
/// the transport for every copy (§13.2.2.4) — and no second BYE.
#[tokio::test(start_paused = true)]
async fn a_straggler_repeat_is_re_acked_and_draws_no_second_bye() {
    let h = Harness::new("b2bua-fork-straggler-repeat");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6062).start(&h, "b2bua", B2BUA).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag(WINNER).await;
    call.expect(180).await;
    uas.adopt_to_tag(WINNER);
    uas.respond(200, "OK").with_sdp(ANSWER_WINNER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    uas.respond(200, "OK").with_to_tag(STRAGGLER).with_sdp(ANSWER_STRAGGLER).await;
    let first = bob.receive("ACK").await;
    let mut straggler_bye = bob.receive("BYE").await;
    straggler_bye.respond(200, "OK").await;
    // The straggler repeats its 2xx: the same ACK answers it, and nothing else.
    uas.respond(200, "OK").with_to_tag(STRAGGLER).with_sdp(ANSWER_STRAGGLER).await;
    let again = bob.receive("ACK").await;
    assert_eq!(
        again.request().image(),
        first.request().image(),
        "the repeat draws the same ACK, byte for byte"
    );
    for _ in 0..5 {
        h.advance(Duration::from_millis(100)).await;
    }
    bob.drain().await;
    assert_eq!(
        datagrams_to_bob_tagged(&h, &b2bua, STRAGGLER),
        3,
        "two ACKs and one BYE: the repeat draws no second BYE",
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR for the one call");
    alice.drain().await;
    bob.drain().await;
    let _report = h.finish().await;
}

/// Datagrams the SUT put on the wire toward bob carrying `tag` in their To.
fn datagrams_to_bob_tagged(h: &Harness, b2bua: &B2buaSut, tag: &str) -> usize {
    h.wire_entries()
        .into_iter()
        .filter(|e| e.from == b2bua.addr && e.to == BOB.parse().unwrap())
        .filter(|e| sip_message::sniff::to_tag(&e.raw) == tag)
        .count()
}

/// The 200 OKs the SUT sent alice for her initial INVITE.
fn alice_finals(h: &Harness, b2bua: &B2buaSut) -> usize {
    h.wire_entries()
        .into_iter()
        .filter(|e| e.from == b2bua.addr && e.to == ALICE.parse().unwrap())
        .filter(|e| e.raw.starts_with(b"SIP/2.0 200 "))
        .filter(|e| sip_message::sniff::cseq_number(&e.raw) == Some(1))
        .count()
}
