//! Transaction-scoped CANCEL of a **re-INVITE** (RFC 3261 §9). CANCEL targets
//! the one pending INVITE transaction it matches: the CANCEL is answered 200
//! and relayed to the peer's pending relayed re-INVITE, whose final, a 487 or
//! a 200 that crossed the CANCEL, answers the canceller (ADR-0065). The
//! established dialog and the call stay intact: an in-dialog CANCEL never
//! begins termination of the whole call.
//!
//! ```text
//!   cancel_reinvite_ends_renegotiation_keeps_call
//!       alice re-INVITE → relayed to bob → alice CANCELs → 200(CANCEL) →
//!       the CANCEL is held until bob's branch has a provisional (§9.1) →
//!       relayed to bob (same txn), bob 200+487 → 487(INVITE) relayed to
//!       alice → call still up → BYE completes
//!   cancel_reinvite_crossing_200_is_relayed_to_the_canceller
//!       bob answers 200 while the CANCEL is still held → the CANCEL dies
//!       with the answered txn (never on the wire); bob's 200 is relayed to
//!       alice, whose ACK reaches bob → call still up
//!   a_delayed_offer_200_crossing_the_cancel_…
//!       the same with an offerless re-INVITE: bob's 200 makes the offer and
//!       alice's ACK answers it
//!   callee_cancel_reinvite_ends_renegotiation_keeps_call
//!       the mirror from the called side: bob re-INVITEs, then CANCELs; the
//!       CANCEL names bob's dialog tag as its From-tag and reaches the call
//!       through the outgoing leg's dialog
//!   cancel_after_reinvite_answered_is_200_and_keeps_call
//!       the re-INVITE was already answered end-to-end → late CANCEL gets 200
//!       (txn layer, §9.2 / RFC 6026 §7.1), call untouched
//! ```
//!
//! The initial-INVITE CANCEL teardown path is unchanged and stays pinned by
//! `cancel_during_slow_decision.rs` / `cancel_200_crossing.rs` /
//! `teardown_races.rs`.

use std::time::Duration;

use b2bua::TurnProbes;
use b2bua_harness::{settle_until, B2buaSut, Pace, RouterPace};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// alice sends a re-INVITE, the B2BUA relays it to bob, and alice CANCELs
/// before bob answers. The B2BUA must (a) answer her CANCEL 200, (b) CANCEL
/// the *relayed* re-INVITE toward bob — same transaction (branch + CSeq) as
/// the relayed re-INVITE — (c) relay bob's 487 to alice as her re-INVITE's
/// final, and (d) keep the dialog and the call alive: a subsequent BYE
/// completes normally on the original dialog state.
#[tokio::test]
async fn cancel_reinvite_ends_renegotiation_keeps_call() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel", 0);
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;

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

    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;

    // ── the B2BUA CANCELs the relayed re-INVITE toward bob — HELD until bob's
    //    branch has a provisional (§9.1), then the SAME transaction it relayed
    //    (CANCEL echoes the INVITE's branch and CSeq number) ──
    bob_reinv_uas.respond(100, "Trying").await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    assert!(alice.take_queued().await.is_none(), "alice's re-INVITE waits for bob's final");
    assert_eq!(
        bob_cxl.request().top_via().branch(),
        relayed_reinvite.top_via().branch(),
        "the CANCEL targets the relayed re-INVITE's transaction (same branch)"
    );
    assert_eq!(
        bob_cxl.request().cseq().seq(),
        relayed_reinvite.cseq().seq(),
        "the CANCEL reuses the relayed re-INVITE's CSeq number"
    );

    // bob is a normal UAS: 200 the CANCEL, 487 the re-INVITE (RFC 3261 §9.2).
    bob_cxl.respond(200, "OK").await;
    bob_reinv_uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    reinv.expect(487).await;

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

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// bob (the called side) re-INVITEs and CANCELs before alice answers. The
/// CANCEL carries bob's From-tag, the remote tag of the outgoing leg's dialog,
/// and resolves to the call by it: the B2BUA CANCELs the re-INVITE it relayed
/// to alice and keeps the call up.
#[tokio::test]
async fn callee_cancel_reinvite_ends_renegotiation_keeps_call() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-callee", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv = bob_dialog.reinvite(Some(REANSWER)).await;
    let mut alice_reinv_uas = alice.receive("INVITE").await;
    let relayed_reinvite = alice_reinv_uas.request().clone();

    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;

    alice_reinv_uas.respond(100, "Trying").await;
    let mut alice_cxl =
        alice.try_receive("CANCEL").await.expect("the B2BUA CANCELs the re-INVITE it relayed");
    assert!(bob.take_queued().await.is_none(), "bob's re-INVITE waits for alice's final");
    assert_eq!(alice_cxl.request().cseq().seq(), relayed_reinvite.cseq().seq());
    alice_cxl.respond(200, "OK").await;
    alice_reinv_uas.respond(487, "Request Terminated").await;
    alice.receive("ACK").await;
    reinv.expect(487).await;
    assert_eq!(b2bua.active_calls(), 1, "re-INVITE CANCEL must not tear the call down");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1 && b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The crossing case: bob's 200 OK to the relayed re-INVITE beats the
/// B2BUA's CANCEL, held for bob's first provisional (RFC 3261 §9.1). The 200
/// is relayed to alice as her re-INVITE's final, as she would meet it end to
/// end (§9.2: the CANCEL had no effect), and her ACK reaches bob: both sides
/// hold the renegotiated session and the call stays up.
#[tokio::test]
async fn cancel_reinvite_crossing_200_is_relayed_to_the_canceller() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;

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
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv_uas = bob.receive("INVITE").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;

    // The B2BUA's CANCEL is HELD — bob's branch has no response yet (§9.1) —
    // and bob commits his 200 OK first (the crossing): the 200 ends the client
    // transaction and the held CANCEL dies with it, never reaching the wire.
    bob_reinv_uas.respond(200, "OK").with_sdp(REANSWER).await;

    let ok = reinv.expect(200).await;
    assert_eq!(ok.body(), REANSWER.as_bytes(), "bob's answer reaches alice");
    dialog.ack_for(reinvite_cseq, None).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().cseq().seq(), bob_reinv_uas.request().cseq().seq());
    assert_eq!(
        bob.drain().await,
        0,
        "the held CANCEL died with the answered transaction — never on the wire"
    );

    // ── the call is STILL alive; a normal BYE completes ──
    assert_eq!(b2bua.active_calls(), 1, "crossing 200 must not kill the call");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// The delayed-offer crossing: alice's re-INVITE carries no offer, she
/// CANCELs it, and bob's 200, which makes his offer, crosses the CANCEL. Only
/// alice can answer that offer, and the ACK must carry the answer (RFC 3261
/// §13.2.2.4). So bob's final decides alice's: she is not answered 487 at
/// once but given bob's 200, as she would be end to end (§9.2: a CANCEL that
/// reaches a UAS after its final has no effect), and her ACK's answer reaches
/// bob. Both sides then hold the same session. bob's 200 leaves half a transit
/// after alice's CANCEL: it reaches the SUT after her CANCEL, and bob answered
/// before the relayed CANCEL reached him.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_200_crossing_the_cancel_is_relayed_and_answered_by_the_canceller() {
    let h = Harness::new("b2bua-reinvite-cancel-crossing-offer");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = dialog.reinvite(None).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    assert!(
        bob_reinv.request().body().is_empty(),
        "the offerless re-INVITE reached bob as it came"
    );
    bob_reinv.respond(100, "Trying").await;
    let mut cxl = reinv.cancel().await;
    h.advance(Duration::from_millis(50)).await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    cxl.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;

    let ok = reinv.expect(200).await;
    assert_eq!(ok.body(), REANSWER.as_bytes(), "bob's offer reaches alice");
    dialog.ack_for(reinvite_cseq, Some(REOFFER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(
        ack.request().cseq().seq(),
        bob_reinv.request().cseq().seq(),
        "§13.2.2.4: the ACK echoes the re-INVITE's CSeq",
    );
    let answer = String::from_utf8_lossy(ack.request().body()).to_string();
    assert!(answer.contains("m=audio 30000 "), "the ACK carries alice's answer: {answer:?}");

    assert_eq!(b2bua.active_calls(), 1, "the crossing 200 keeps the call up");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// alice CANCELs her re-INVITE, then hangs up before bob's final: her
/// re-INVITE still holds no final, so her BYE draws this stack's 487 to it
/// (RFC 3261 §15.1.2), and the CANCEL already on its way to bob is not sent
/// again. bob's late 487 is ACKed and goes no further.
#[tokio::test(start_paused = true)]
async fn a_bye_after_the_relayed_cancel_answers_the_reinvite_487_without_a_second_cancel() {
    let h = Harness::new("b2bua-reinvite-cancel-then-bye");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(100, "Trying").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    assert!(alice.take_queued().await.is_none(), "alice's re-INVITE waits for bob's final");

    let mut bye = dialog.bye().await;
    bye.expect(200).await;
    reinv.expect(487).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bob_reinv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    let report = h.finish().await;
    let cancels_to_bob = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob.addr() && e.raw.starts_with(b"CANCEL "))
        .count();
    assert_eq!(cancels_to_bob, 1, "one CANCEL toward bob: the BYE sends no second one");
    b2bua.assert_fully_reaped();
}

/// alice CANCELs her re-INVITE and bob never answers it, nor the CANCEL: the
/// relayed re-INVITE's client transaction gives up 64·T1 past the CANCEL (RFC
/// 3261 §9.1), not at the INVITE's own later bound. bob's dialog timed out
/// (§12.2.1.2), so the call ends: alice's re-INVITE takes the teardown's 487
/// (§15.1.2) and both dialogs a BYE.
#[tokio::test(start_paused = true)]
async fn a_relayed_cancel_bob_never_answers_ends_at_64_t1_past_the_cancel() {
    let h = Harness::new("b2bua-reinvite-cancel-unanswered");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5077)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5087")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(100, "Trying").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    let _unanswered = bob.receive("CANCEL").await;
    assert!(alice.take_queued().await.is_none(), "alice's re-INVITE waits for bob's final");
    let cancelled_at = tokio::time::Instant::now();

    h.advance(Duration::from_millis(sip_txn::timers::TIMER_B + 1_000)).await;
    reinv.expect(487).await;
    assert!(
        cancelled_at.elapsed() < Duration::from_millis(2 * sip_txn::timers::TIMER_B),
        "the give-up runs from the CANCEL, not from the INVITE's own bound"
    );
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive_absorbing("BYE", &["CANCEL"]).await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// alice's CANCEL reaches the call only after the transaction layer's bound
/// on its held answers passed (the router held for half a second): the bound
/// sends the CANCEL's 200 alone and keeps alice's re-INVITE open for the turn
/// that relays the CANCEL, so bob's crossing 200 still answers her, her ACK's
/// answer reaches bob, and no 487 ever reaches her (RFC 3261 §17.2.1: one
/// final per transaction).
#[tokio::test(start_paused = true)]
async fn a_cancel_turn_past_the_layers_bound_still_relays_the_crossing_200() {
    cancel_turn_past_the_bound("b2bua-reinvite-cancel-past-the-bound", 350, 150).await;
}

/// The same with the turn running more than T1 late (bob's 200 650 ms after
/// alice's CANCEL): the wire is the same, and the relayed 200 is no
/// §9.2 violation of this stack's.
#[tokio::test(start_paused = true)]
async fn a_cancel_turn_past_t1_still_relays_the_crossing_200() {
    cancel_turn_past_the_bound("b2bua-reinvite-cancel-past-t1", 650, 100).await;
}

/// alice's CANCEL taken with the router held; bob's crossing 200 sent
/// `before_200_ms` after it, the router freed `after_200_ms` later.
async fn cancel_turn_past_the_bound(name: &str, before_200_ms: u64, after_200_ms: u64) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let pace = RouterPace::new();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5078)
        .turn_probe(TurnProbes::new(pace.clone()))
        .start(&h, "b2bua", "127.0.0.1:5088")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = dialog.reinvite(None).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(100, "Trying").await;
    h.advance(Duration::from_millis(20)).await;

    pace.set("w0", Pace::Frozen);
    let mut cxl = reinv.cancel().await;
    h.advance(Duration::from_millis(before_200_ms)).await;
    cxl.expect(200).await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    h.advance(Duration::from_millis(after_200_ms)).await;
    pace.set("w0", Pace::Free);

    let ok = reinv.expect(200).await;
    assert_eq!(ok.body(), REANSWER.as_bytes(), "bob's offer reaches alice");
    dialog.ack_for(reinvite_cseq, Some(REOFFER)).await;
    let ack = bob.receive("ACK").await;
    let answer = String::from_utf8_lossy(ack.request().body()).to_string();
    assert!(answer.contains("m=audio 30000 "), "the ACK carries alice's answer: {answer:?}");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let terminated = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == alice.addr())
        .filter(|e| e.raw.starts_with(b"SIP/2.0 487 "))
        .count();
    assert_eq!(terminated, 0, "alice's re-INVITE takes bob's 200 as its one final");
}

/// A CANCEL that arrives after the re-INVITE was answered end-to-end (bob's
/// 200 already relayed to alice) matches a server INVITE transaction held past
/// its final: the txn layer answers it 200 (RFC 3261 §9.2, RFC 6026 §7.1) and
/// the established call is untouched — the in-dialog analog of
/// `cancel_after_answer.rs`.
#[tokio::test]
async fn cancel_after_reinvite_answered_is_200_and_keeps_call() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-late", 0);
    // Alice's post-ACK CANCEL of the completed re-INVITE violates RFC 3261
    // §9.1 by design — the buggy caller this test exists to absorb — so it is
    // waived on alice only and every B2BUA-side finding stays gated.
    h.waive(
        WaiverScope::rule(
            "no-cancel-after-final",
            "alice deliberately CANCELs a re-INVITE she already ACKed the 200 for \
             (RFC 3261 §9.1) — the late-CANCEL misbehaviour whose absorption this \
             test pins",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

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

    // ── a LATE CANCEL for the completed re-INVITE: 200, nothing else ──
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;

    // ── call untouched; BYE completes ──
    assert_eq!(b2bua.active_calls(), 1, "late re-INVITE CANCEL is a no-op");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
