//! **RFC 3262 §4 — a reliable provisional crossing the B2BUA's CANCEL is
//! PRACKed.** The CANCEL does not end the INVITE transaction (RFC 3261 §9.1):
//! until the callee's final arrives, every reliable provisional it sends is
//! owed a PRACK on its own early dialog, however soon the 487 follows. The
//! caller already took 200 (CANCEL) and 487 (INVITE), so the provisional is
//! shown to no one and the B2BUA, the leg's UAC, acknowledges it itself.
//!
//! ```text
//!   alice            b2bua             bob
//!     INVITE(100rel) →   INVITE(100rel) →
//!                    ←   100
//!     CANCEL →
//!     ← 200 ; ← 487      CANCEL →
//!     ACK →          ←   183(100rel, RSeq)   [crossing the CANCEL]
//!                        PRACK →             [RAck: RSeq INVITE-CSeq INVITE]
//!                    ←   200(CANCEL) ; ← 200(PRACK) ; ← 487
//!                        ACK →
//! ```

use std::time::Duration;

use b2bua_harness::{settle_until, stated, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's own `RSeq` for its reliable provisional.
const BOB_RSEQ: u32 = 731;

/// Bob's 183 crosses the B2BUA's CANCEL: the B2BUA PRACKs it on its early
/// dialog (its To-tag, `RAck: <RSeq> <INVITE CSeq> INVITE`, the dialog's next
/// CSeq) before the 487 settles the leg.
#[tokio::test(start_paused = true)]
async fn a_reliable_183_crossing_the_cancel_is_pracked_before_the_487() {
    let h = Harness::with_transit_delay("b2bua-reliable-1xx-crossing-cancel", 1);
    let alice = h.agent("alice", "127.0.0.1:5191").await;
    let bob = h.agent("bob", "127.0.0.1:5192").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5192).start(&h, "b2bua", "127.0.0.1:5193").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut b_inv = bob.receive("INVITE").await;
    let invite_cseq = b_inv.request().cseq().seq();
    b_inv.respond(100, "Trying").await;
    h.advance(Duration::from_millis(20)).await;

    // ── alice CANCELs; the B2BUA releases her and CANCELs bob's INVITE ──
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── bob's reliable 183 crosses the CANCEL on the wire ──
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(ANSWER)
        .await;
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;

    // ── the B2BUA acknowledges the provisional on its own early dialog ──
    let mut prack = bob.receive("PRACK").await;
    let req = prack.request();
    assert_eq!(req.to().tag(), Some("bob-early"), "the PRACK rides the 183's early dialog");
    assert_eq!(
        stated(req, "RAck").as_deref(),
        Some(format!("{BOB_RSEQ} {invite_cseq} INVITE").as_str()),
        "RAck names the 183's RSeq and the INVITE it answers (RFC 3262 §7.2)"
    );
    assert_eq!(req.cseq().seq(), invite_cseq + 1, "the early dialog's next CSeq");
    prack.respond(200, "OK").await;

    b_inv.respond(487, "Request Terminated").with_to_tag("bob-early").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    assert!(cdrs[0].termination.is_some(), "the cancelled call terminated");

    let _report = h.finish().await;
}

/// The B2BUA holds its CANCEL until the callee's first provisional (RFC 3261
/// §9.1); when that provisional is reliable, the CANCEL it releases and the
/// PRACK it is owed both reach the callee before the 487.
#[tokio::test(start_paused = true)]
async fn a_reliable_183_releasing_a_held_cancel_is_pracked_before_the_487() {
    let h = Harness::with_transit_delay("b2bua-reliable-1xx-releases-held-cancel", 1);
    let alice = h.agent("alice", "127.0.0.1:5194").await;
    let bob = h.agent("bob", "127.0.0.1:5195").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5195).start(&h, "b2bua", "127.0.0.1:5196").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut b_inv = bob.receive("INVITE").await;
    let invite_cseq = b_inv.request().cseq().seq();

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── bob's first word is a reliable 183: it releases the held CANCEL ──
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(ANSWER)
        .await;
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        stated(prack.request(), "RAck").as_deref(),
        Some(format!("{BOB_RSEQ} {invite_cseq} INVITE").as_str()),
    );
    prack.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").with_to_tag("bob-early").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one record");

    let _report = h.finish().await;
}

/// RFC 3262 §4: a reliable provisional whose `RSeq` is not one higher than the
/// last one taken on its dialog is not PRACKed (nor processed further). The
/// callee skips a number after its crossing 183 was PRACKed; the skipped-to
/// 183 draws no PRACK, and the 487 still settles the leg.
#[tokio::test(start_paused = true)]
async fn an_out_of_order_reliable_provisional_crossing_the_cancel_is_not_pracked() {
    let h = Harness::with_transit_delay("b2bua-reliable-1xx-crossing-cancel-rseq-gap", 1);
    h.waive(
        WaiverScope::rule(
            "non-contiguous-rseq",
            "bob skips an RSeq (RFC 3262 §3) — the out-of-order provisional is this test's subject",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:5201").await;
    let bob = h.agent("bob", "127.0.0.1:5202").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5202).start(&h, "b2bua", "127.0.0.1:5203").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(100, "Trying").await;
    h.advance(Duration::from_millis(20)).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    let reliable_183 = |rseq: u32| rseq.to_string();
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &reliable_183(BOB_RSEQ))
        .with_sdp(ANSWER)
        .await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    // ── one number skipped ──
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("bob-early")
        .with_header("Require", "100rel")
        .with_header("RSeq", &reliable_183(BOB_RSEQ + 2))
        .await;
    h.advance(Duration::from_millis(100)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "an out-of-order reliable provisional is never PRACKed (RFC 3262 §4)"
    );
    b_inv.respond(487, "Request Terminated").with_to_tag("bob-early").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
