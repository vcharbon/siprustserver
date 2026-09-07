//! Per-fork CSeq on a forked callee leg (RFC 3261 §12.2.1.1, §12.1.2).
//!
//! One INVITE creates one early dialog per callee To-tag, and **each carries its
//! own local sequence, seeded by that INVITE**. The next request in EACH fork is
//! therefore `INVITE_CSeq + 1`, two forks legitimately carry the same number, and
//! spending a number in one fork owes nothing to any other. A UAC running one
//! counter across a leg's forks skips in every fork but the one it last sent in —
//! a MUST violation §12.2.2 has the receiver tolerate (only a LOWER number draws
//! a 500), so it ships unnoticed against every tolerant peer.
//!
//! bob stands in for a forking proxy: two reliable 183s under distinct To-tags,
//! traffic on fork 1, and the answer on fork 2 — the shape charged on the wire in
//! `capture_122983-case2` (issue 245), where the post-answer re-INVITE went out
//! at CSeq 3 in a fork whose own next number was 2.
//!
//! ```text
//!   INVITE(offer)                                     b-leg CSeq 1, seeds both forks
//!     → 183(fork1, 100rel RSeq 1) → PRACK(fork1)      fork1 CSeq 2
//!     → UPDATE(fork1, re-offer)   → 200               fork1 CSeq 3
//!     → 183(fork2, 100rel RSeq 1) → PRACK(fork2)      fork2 CSeq 2  ← same as fork1's
//!     → 200(INVITE, fork2) → ACK                      fork2 CSeq 1  (§13.2.2.4)
//!     → re-INVITE → 200 → ACK → BYE → 200             fork2 CSeq 3, then 4
//! ```

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER_F1: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER_F2: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const REOFFER_HOLD: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const REANSWER_HELD: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
const REINVITE_RESUME: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const REINVITE_ANSWER: &str = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

fn rseq_of(resp: &SipResponse) -> u32 {
    resp.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value()
}

/// The b-leg's two early dialogs each number their own requests from the one
/// INVITE that created them, and answering on fork 2 leaves fork 2's sequence
/// where fork 2 itself left it — not where fork 1's traffic pushed the leg.
#[tokio::test]
async fn each_callee_fork_numbers_its_requests_from_its_own_invite() {
    let h = Harness::with_transit_delay("b2bua-forked-callee-per-fork-cseq", 1);
    let alice = h.agent("alice", "127.0.0.1:6031").await;
    let bob = h.agent("bob", "127.0.0.1:6032").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6032).start(&h, "b2bua", "127.0.0.1:6033").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    assert_eq!(invite_cseq, 1, "the b-leg INVITE seeds every fork at CSeq 1");

    // ── fork 1 rings, and alice PRACKs it: fork 1's own INVITE + 1 ───────────
    uas.respond(183, "Session Progress").with_to_tag("bobfork1").reliable(1).with_sdp(ANSWER_F1).await;
    let p1 = call.expect(183).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();

    let mut prack1 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork1_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p1)))
        .send()
        .await;
    let mut prack1_at_bob = bob.receive("PRACK").await;
    assert_eq!(prack1_at_bob.request().to().tag(), Some("bobfork1"), "PRACK rides fork 1");
    assert_eq!(
        prack1_at_bob.request().cseq().seq(),
        2,
        "fork 1's first request is its own INVITE's CSeq + 1",
    );
    prack1_at_bob.respond(200, "OK").await;
    prack1.expect(200).await;

    // ── alice holds the early media on fork 1 (RFC 3311 §5.1) ────────────────
    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&fork1_atag)
        .with_sdp(REOFFER_HOLD)
        .send()
        .await;
    let mut update_at_bob = bob.receive("UPDATE").await;
    assert_eq!(update_at_bob.request().to().tag(), Some("bobfork1"), "UPDATE rides fork 1");
    assert_eq!(
        update_at_bob.request().cseq().seq(),
        3,
        "fork 1 continues its own sequence: INVITE 1, PRACK 2, UPDATE 3",
    );
    update_at_bob.respond(200, "OK").with_sdp(REANSWER_HELD).await;
    update.expect(200).await;

    // ── fork 2 rings: its sequence is untouched by everything fork 1 spent ───
    uas.respond(183, "Session Progress").with_to_tag("bobfork2").reliable(1).with_sdp(ANSWER_F2).await;
    let p2 = call.expect(183).await;
    let fork2_atag = p2.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(fork1_atag, fork2_atag, "each callee fork maps to a distinct a-facing tag");

    let mut prack2 = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&fork2_atag)
        .with_rack(&format!("{} 1 INVITE", rseq_of(&p2)))
        .send()
        .await;
    let mut prack2_at_bob = bob.receive("PRACK").await;
    assert_eq!(prack2_at_bob.request().to().tag(), Some("bobfork2"), "PRACK rides fork 2");
    assert_eq!(
        prack2_at_bob.request().cseq().seq(),
        2,
        "fork 2's first request is ITS OWN INVITE's CSeq + 1 — the same number \
         fork 1's PRACK carried, which two distinct dialogs may legitimately share",
    );
    prack2_at_bob.respond(200, "OK").await;
    prack2.expect(200).await;

    // ── fork 2 answers; the ACK echoes the INVITE's CSeq (§13.2.2.4) ─────────
    uas.respond(200, "OK").with_to_tag("bobfork2").await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(fork2_atag.as_str()), "the winning fork's a-facing tag");
    let mut dialog = call.ack().await;
    let ack_at_bob = bob.receive("ACK").await;
    assert_eq!(ack_at_bob.request().to().tag(), Some("bobfork2"), "ACK rides fork 2");
    assert_eq!(ack_at_bob.request().cseq().seq(), 1, "the 2xx ACK reuses the INVITE's CSeq");

    // ── confirming fork 2 does not renumber it off fork 1's traffic ──────────
    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(REINVITE_RESUME)).await;
    let mut reinvite_at_bob = bob.receive("INVITE").await;
    assert_eq!(reinvite_at_bob.request().to().tag(), Some("bobfork2"), "re-INVITE rides fork 2");
    assert_eq!(
        reinvite_at_bob.request().cseq().seq(),
        3,
        "fork 2 spent 1 (INVITE) and 2 (PRACK), so its next request is 3 — \
         fork 1's UPDATE at 3 belongs to another dialog and owes this one nothing",
    );
    reinvite_at_bob.respond(200, "OK").with_sdp(REINVITE_ANSWER).await;
    reinvite.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;

    // ── teardown ────────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().to().tag(), Some("bobfork2"), "BYE rides fork 2");
    assert_eq!(
        bye_at_bob.request().cseq().seq(),
        4,
        "fork 2's sequence stays contiguous to the end: 1, 2, 3, then 4",
    );
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The shape charged on the wire (`capture_122983-case2`, issue 245): both
/// provisionals ring **one** callee tag and the 2xx answers under a **second**
/// one that never rang. §12.1.2 has that 2xx create its own dialog, seeded like
/// every other by the INVITE that drew it — so the next request in it is CSeq 2,
/// however much the rung fork spent. Confirming an unrung tag onto the rung
/// fork's record hands the answer a sequence it never had.
///
/// ```text
///   INVITE(offer)                                 b-leg CSeq 1
///     → 180(fork1) → UPDATE(fork1) → 200          fork1 CSeq 2
///     → 183(fork1, early-media preview)
///     → 200(INVITE, fork2 — never rung, answer)   fork2 CSeq 1 (§13.2.2.4)
///     → ACK → re-INVITE → 200 → ACK → BYE → 200   fork2 CSeq 2, then 3
/// ```
#[tokio::test]
async fn an_answer_under_an_unrung_tag_starts_its_own_sequence() {
    let h = Harness::with_transit_delay("b2bua-answer-under-unrung-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:6034").await;
    let bob = h.agent("bob", "127.0.0.1:6035").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 6035).start(&h, "b2bua", "127.0.0.1:6036").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().cseq().seq(), 1, "the b-leg INVITE seeds every dialog it creates at 1");

    // ── fork 1 rings, and alice refreshes the early dialog on it ────────────
    // The UPDATE carries no offer: alice's INVITE offer is still unanswered
    // (fork 1 rang bodiless), and a second offer before the first is answered is
    // no offer at all (RFC 3264 §4). What it does carry is fork 1's next CSeq.
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let p1 = call.expect(180).await;
    let fork1_atag = p1.to().tag().expect("fork1 a-facing tag").to_string();

    let mut update =
        call.send_request(InDialogMethod::Update).with_to_tag(&fork1_atag).send().await;
    let mut update_at_bob = bob.receive("UPDATE").await;
    assert_eq!(update_at_bob.request().to().tag(), Some("bobfork1"), "UPDATE rides fork 1");
    assert_eq!(update_at_bob.request().cseq().seq(), 2, "fork 1's own INVITE + 1");
    update_at_bob.respond(200, "OK").await;
    update.expect(200).await;

    uas.respond(183, "Session Progress").with_to_tag("bobfork1").with_sdp(ANSWER_F1).await;
    call.expect(183).await;

    // ── a second branch answers outright, under a tag nothing has rung ──────
    // Fork 1's 183 previewed early media on ITS dialog; the answer to alice's
    // offer is owed by the first reliable response on the dialog that takes the
    // call, and this 2xx is it (RFC 3264 §3).
    uas.respond(200, "OK").with_to_tag("bobfork2").with_sdp(ANSWER_F2).await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        ANSWER_F2,
        "the answering fork's own answer reaches alice on the 2xx",
    );
    let mut dialog = call.ack().await;
    let ack_at_bob = bob.receive("ACK").await;
    assert_eq!(ack_at_bob.request().to().tag(), Some("bobfork2"), "the ACK rides the answering dialog");
    assert_eq!(ack_at_bob.request().cseq().seq(), 1, "the 2xx ACK reuses the INVITE's CSeq");

    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(REINVITE_RESUME)).await;
    let mut reinvite_at_bob = bob.receive("INVITE").await;
    assert_eq!(reinvite_at_bob.request().to().tag(), Some("bobfork2"), "re-INVITE rides the answering dialog");
    assert_eq!(
        reinvite_at_bob.request().cseq().seq(),
        2,
        "the answering dialog spent only its INVITE, so its next request is 2 — \
         the UPDATE at 2 belongs to fork 1 and owes this dialog nothing",
    );
    reinvite_at_bob.respond(200, "OK").with_sdp(REINVITE_ANSWER).await;
    reinvite.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().cseq().seq(), 3, "the answering dialog stays contiguous: 1, 2, then 3");
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
