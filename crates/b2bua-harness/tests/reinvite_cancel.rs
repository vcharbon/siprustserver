//! Transaction-scoped CANCEL of a **re-INVITE** (RFC 3261 §9). CANCEL targets
//! the one pending INVITE transaction it matches: cancelling a re-INVITE ends
//! that renegotiation — 487 to the re-INVITE, CANCEL relayed to the peer's
//! pending relayed re-INVITE — and leaves the established dialog and the call
//! intact. Pre-fix, `handle-cancel` unconditionally `BeginTermination`d on ANY
//! CANCEL, so an in-dialog CANCEL killed the whole call.
//!
//! ```text
//!   cancel_reinvite_ends_renegotiation_keeps_call
//!       alice re-INVITE → relayed to bob → alice CANCELs →
//!       CANCEL relayed to bob (same txn), bob 200+487 →
//!       alice 200(CANCEL)+487(INVITE) → call still up → BYE completes
//!   cancel_reinvite_crossing_200_is_acked_and_absorbed
//!       bob answers 200 while the CANCEL is in flight → B2BUA ACKs bob,
//!       relays nothing to alice (she has her 487) → call still up
//!   cancel_after_reinvite_answered_is_481_and_keeps_call
//!       the re-INVITE was already answered end-to-end → late CANCEL gets 481
//!       (txn layer, §9.2), call untouched
//! ```
//!
//! The initial-INVITE CANCEL teardown path is unchanged and stays pinned by
//! `cancel_during_slow_decision.rs` / `cancel_200_crossing.rs` /
//! `teardown_races.rs`.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// alice sends a re-INVITE, the B2BUA relays it to bob, and alice CANCELs
/// before bob answers. The B2BUA must (a) let the txn layer answer alice
/// (200 to the CANCEL, 487 to the re-INVITE), (b) CANCEL the *relayed*
/// re-INVITE toward bob — same transaction (branch + CSeq) as the relayed
/// re-INVITE — and (c) keep the dialog and the call alive: a subsequent BYE
/// completes normally on the original dialog state.
#[tokio::test]
async fn cancel_reinvite_ends_renegotiation_keeps_call() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel", 0);
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── alice re-INVITEs; bob receives the relayed re-INVITE but does not answer ──
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv_uas = bob.receive("INVITE").await;
    let relayed_reinvite = bob_reinv_uas.request().clone();

    // ── alice CANCELs the pending re-INVITE ──
    // The txn layer answers alice immediately: 200 OK to the CANCEL, then 487
    // Request Terminated on the re-INVITE — both on the established dialog's
    // To-tag (RFC 3261 §9.1). The call must NOT be torn down.
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    reinv.expect(487).await;

    // ── the B2BUA CANCELs the relayed re-INVITE toward bob — the SAME
    //    transaction it relayed (§9.1: CANCEL echoes the INVITE's branch and
    //    CSeq number) ──
    let mut bob_cxl = bob.receive("CANCEL").await;
    assert_eq!(
        bob_cxl.request().via.first().branch,
        relayed_reinvite.via.first().branch,
        "the CANCEL targets the relayed re-INVITE's transaction (same branch)"
    );
    assert_eq!(
        bob_cxl.request().cseq.seq,
        relayed_reinvite.cseq.seq,
        "the CANCEL reuses the relayed re-INVITE's CSeq number"
    );

    // bob is a normal UAS: 200 the CANCEL, 487 the re-INVITE (RFC 3261 §9.2).
    bob_cxl.respond(200, "OK").await;
    bob_reinv_uas.respond(487, "Request Terminated").await;
    // The B2BUA's client transaction auto-ACKs bob's 487 (§17.1.1.3).
    bob.receive("ACK").await;

    // ── the call is STILL alive: the cancelled renegotiation must not have
    //    touched the established dialog ──
    assert_eq!(b2bua.active_calls(), 1, "re-INVITE CANCEL must not tear the call down");

    // A subsequent BYE from alice completes normally with the original dialog
    // state — proof the session survived the cancelled renegotiation.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // ── exactly one CDR: answered, renegotiation-cancelled, then BYE'd ──
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one call, one CDR");
    let kinds: Vec<call::CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&call::CdrEventType::Answer), "answered: {kinds:?}");
    assert!(kinds.contains(&call::CdrEventType::Bye), "BYE'd: {kinds:?}");
    assert!(
        cdrs[0].events.iter().any(|e| e.reason.as_deref() == Some("reinvite_cancelled")),
        "the cancelled renegotiation is recorded: {:?}",
        cdrs[0].events
    );

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// The crossing case: bob's 200 OK to the relayed re-INVITE crosses the
/// B2BUA's CANCEL on the wire (RFC 3261 §9.1 — cancellation is best-effort;
/// the UAS may have already committed its final). Alice was already 487'd by
/// the txn layer, so the B2BUA must NOT relay the 200; it ACKs bob (quiescing
/// his 2xx retransmissions, §13.2.2.4) and keeps the call up. The two sides'
/// SDP views may diverge until the next renegotiation — the documented
/// minimal-intervention choice (the alternative, killing the call or minting
/// a resync re-INVITE, costs more than the transient divergence).
#[tokio::test]
async fn cancel_reinvite_crossing_200_is_acked_and_absorbed() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs; bob receives it; alice CANCELs ──
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv_uas = bob.receive("INVITE").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    reinv.expect(487).await;

    // The relayed CANCEL arrives at bob — but bob had already committed his
    // 200 OK (the crossing): he answers the re-INVITE 200 anyway, and the
    // CANCEL "has no effect" beyond its own 200 (RFC 3261 §9.2).
    let mut bob_cxl = bob.receive("CANCEL").await;
    bob_reinv_uas.respond(200, "OK").with_sdp(REANSWER).await;
    bob_cxl.respond(200, "OK").await;

    // ── the B2BUA ACKs bob's crossing 200 on the confirmed dialog (re-INVITE
    //    CSeq, §13.2.2.4) instead of relaying it — alice already has her 487 ──
    bob.receive("ACK").await;
    assert_eq!(
        alice.drain().await,
        0,
        "nothing further toward alice — the crossing 200 is absorbed, not relayed"
    );

    // ── the call is STILL alive; a normal BYE completes ──
    assert_eq!(b2bua.active_calls(), 1, "crossing 200 must not kill the call");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// A CANCEL that arrives after the re-INVITE was answered end-to-end (bob's
/// 200 already relayed to alice) matches no active INVITE server transaction:
/// the txn layer rejects it 481 (RFC 3261 §9.2) and the established call is
/// untouched — the in-dialog analog of `cancel_after_answer.rs`.
#[tokio::test]
async fn cancel_after_reinvite_answered_is_481_and_keeps_call() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-late", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── the re-INVITE completes normally ──
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await;

    // ── a LATE CANCEL for the completed re-INVITE: 481, nothing else ──
    let mut cxl = reinv.cancel().await;
    cxl.expect(481).await;

    // ── call untouched; BYE completes ──
    assert_eq!(b2bua.active_calls(), 1, "late re-INVITE CANCEL is a no-op");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
