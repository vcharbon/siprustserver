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
//! The B2BUA confirms the winner and takes nothing from the straggler —
//! `re-ack-retransmitted-2xx` declines a To-tag no dialog on the leg carries
//! (`reack_retransmitted_2xx::a_foreign_tagged_2xx_is_not_a_retransmission`).
//! What this cell asserts is that refusing it costs our platform nothing: the
//! caller keeps the one answer she was given, the hangup BYE addresses the
//! WINNING tag, the CDR is written, and the call record reaps empty.

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
/// afterwards on the same INVITE CSeq under its own tag. The B2BUA ignores the
/// straggler and its own call stays exactly as it was — one answer to alice, a
/// hangup BYE on the winner's tag, a CDR, and a reaped record.
#[tokio::test(start_paused = true)]
async fn a_fork_straggler_leaves_our_call_intact_and_reaped() {
    let h = Harness::new("b2bua-fork-straggler-no-leak");
    // The straggler is the deliberate peer-side non-compliance: a second remote
    // UA's answer that this call refuses, whose own dialog the scripted peer
    // then neither retransmits to Timer H nor BYEs (§13.3.1.4 is the answerer's
    // duty, and the answerer is behind the fork, not on this fabric).
    h.allow_violation(
        "unacked-2xx-not-cleared",
        "the scripted fork straggler answers under a tag this call never confirmed and does not clean up its own dialog",
    );
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
    for _ in 0..5 {
        h.advance(Duration::from_millis(100)).await;
    }
    bob.drain().await;

    // Nothing this call sends belongs to the straggler's dialog.
    assert_eq!(
        datagrams_to_bob_tagged(&h, &b2bua, STRAGGLER),
        0,
        "the straggler's dialog draws nothing from us",
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
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
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
        0,
        "the whole call through, we addressed the straggler's dialog never",
    );

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
