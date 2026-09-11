//! RFC 5407 (BCP 147) §2 — the **Mortal** state: a UA that has sent or received
//! a BYE "MUST NOT send any new requests within the dialog", and the ACK for a
//! 2xx is carved straight back out of that ban. The ACK belongs to the INVITE
//! transaction, not to the dialog (RFC 3261 §6, §17.1), so the invite usage is
//! kept past the BYE for exactly one purpose: emitting that ACK
//! (RFC 5407 Appendix D).
//!
//! The cost of getting it wrong is observable: RFC 3261 §13.3.1.4 has the
//! answerer repeat its 2xx until the ACK arrives — 64·T1, ~32 s — and then §14.2
//! BYE a dialog that is already gone.
//!
//! Which ACKs can a BYE even cross? §13.2.2.4 puts the ACK in the UAC core, so
//! this stack emits it the moment it takes the 2xx — there is no window. The
//! **delayed-offer** shape is the sole exception: its ACK carries the answer
//! only the far party's own ACK supplies (RFC 3264 §4), so it waits, and a BYE
//! can arrive while it is still owed. That is what the cross scenarios below
//! are built from; `the_initial_2xx_is_acked_on_receipt_so_no_bye_can_cross_it`
//! pins the other half — that the window is genuinely closed everywhere else.
//!
//! ```text
//!   acked_on_receipt_…  the offer-carrying 2xx: ACKed before a BYE can cross
//!   callee_byes_…       the callee BYEs; the SUT still ACKs (delayed offer)
//!   caller_byes_…       the caller BYEs, so the SUT is Mortal by its own act
//!   late_inbound_…      the receiving half: a late ACK into a reaped dialog is
//!                       absorbed, never answered (§17.1.1.3 — an ACK draws no
//!                       response at all, least of all a 481)
//!   repeated_…          a repeat across the BYE still draws its own ACK
//! ```

use std::net::SocketAddr;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, RunReport};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Every ACK the SUT put on `callee`'s socket, in wire order.
fn acks_to(report: &RunReport, sut: SocketAddr, callee: SocketAddr) -> Vec<Vec<u8>> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == callee && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .collect()
}

/// **The window is closed.** alice's INVITE carries the offer, so the ACK for
/// bob's 200 owes no body and the UAC core composes it from the dialog alone:
/// it is on the wire before alice has said anything, and bob cannot BYE across
/// an ACK he already holds. alice's own ACK is then hop-local — relaying it
/// would put a second ACK on a transaction that is already quiesced.
#[tokio::test]
async fn the_initial_2xx_is_acked_on_receipt_so_no_bye_can_cross_it() {
    const BOB: &str = "127.0.0.1:5071";
    let h = Harness::with_transit_delay("b2bua-initial-ack-on-receipt", 0);
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5071).start(&h, "b2bua", "127.0.0.1:5081").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // ── the ACK, with alice still silent (§13.2.2.4: one per 2xx RECEIVED) ───
    let ack = bob.receive("ACK").await;
    assert_eq!(
        ack.request().cseq().seq(),
        uas.request().cseq().seq(),
        "§13.2.2.4: the ACK echoes the INVITE's CSeq",
    );
    assert!(ack.request().body().is_empty(), "an offer-in-INVITE call's ACK carries no body");

    // ── bob hangs up on an answer he HAS been ACKed for ──────────────────────
    let mut bob_dialog = uas.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    let mut alice_bye = alice.receive("BYE").await;

    // ── alice's ACK lands last and goes nowhere ──────────────────────────────
    let _alice_dialog = call.ack().await;
    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    assert_eq!(
        acks_to(&report, b2bua.addr, bob_addr).len(),
        1,
        "one 2xx received, one ACK sent — the caller's own ACK is hop-local",
    );
}

/// **The dialog-creating 2xx, delayed offer.** alice INVITEs bodyless, so bob's
/// 200 holds the offer and the ACK owes the answer only alice's ACK supplies
/// (RFC 3264 §4) — the one initial-INVITE shape a BYE can still cross. bob
/// hangs up first; the SUT is Mortal on a b-leg whose invite usage is not even
/// confirmed yet, and still owes the ACK that completes the handshake.
#[tokio::test]
async fn callee_byes_across_the_delayed_offer_initial_2xx_and_the_sut_still_acks() {
    let h = Harness::with_transit_delay("b2bua-bye-initial-ack", 0);
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(
        uas.request().body().is_empty(),
        "the offerless INVITE reached bob with a substituted body"
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    assert_eq!(b2bua.active_calls(), 1, "the call is answered");

    // ── bob hangs up on the answer he has not been ACKed for ─────────────────
    let mut bob_dialog = uas.dialog();
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    let mut alice_bye = alice.receive("BYE").await;

    // ── alice supplies the answer at last; the ACK must still reach bob ──────
    let _alice_dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert!(
        !ack.request().body().is_empty(),
        "the b-leg ACK must carry alice's answer to bob's offer"
    );

    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **The callee hangs up inside a delayed-offer re-INVITE transaction.** alice
/// re-INVITEs bodyless, bob answers 200 with the offer and BYEs before the ACK
/// reaches him. The SUT is Mortal on the b-leg — it took the BYE — and still
/// owes bob the ACK for a 2xx it received (RFC 5407 §2 + Appendix D,
/// RFC 3261 §13.2.2.4). Destroying the b-leg dialog on the BYE would swallow it
/// and leave bob laddering his 200.
#[tokio::test]
async fn callee_byes_across_the_reinvite_2xx_and_the_sut_still_acks() {
    let h = Harness::with_transit_delay("b2bua-bye-reinvite-ack-callee", 0);
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    let (mut alice_dialog, mut bob_dialog) = establish(&alice, &bob, &b2bua).await;

    // ── alice re-INVITEs bodyless; bob's 200 makes the offer ─────────────────
    let mut reinv = alice_dialog.reinvite(None).await;
    let reinvite_cseq = alice_dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    assert!(
        bob_reinv.request().body().is_empty(),
        "the offerless re-INVITE reached bob with a substituted body"
    );
    let b_leg_reinvite_branch = bob_reinv.request().top_via().branch().unwrap().to_string();
    let b_leg_call_id = bob_reinv.request().call_id().as_str().to_string();
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;

    // ── bob hangs up before the ACK gets to him ──────────────────────────────
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    let mut alice_bye = alice.receive("BYE").await;

    // ── alice ACKs the re-INVITE 2xx with her answer; the SUT is Mortal on
    //    both legs and must still put that ACK on the wire toward bob ─────────
    alice_dialog.ack_for(reinvite_cseq, Some(REOFFER)).await;
    let ack_txn = bob.receive("ACK").await;
    let ack = ack_txn.request();
    assert_eq!(
        ack.cseq().seq(),
        bob_reinv.request().cseq().seq(),
        "§13.2.2.4: the ACK echoes the re-INVITE's CSeq",
    );
    assert!(!ack.body().is_empty(), "the ACK carries the answer only alice's ACK could supply");
    assert_eq!(ack.call_id().as_str(), b_leg_call_id, "the ACK rides the b-leg dialog");
    assert_ne!(
        ack.top_via().branch().unwrap(),
        b_leg_reinvite_branch,
        "§17.1.1.3: a 2xx ACK is its own transaction, so it carries a fresh branch",
    );

    // ── the teardown completes normally on top of the ACK ────────────────────
    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **The caller hangs up inside the re-INVITE transaction** — RFC 5407 §3.2.3's
/// depicted flow, on the delayed-offer shape that still has an ACK in flight.
/// alice re-INVITEs bodyless, bob answers 200, alice BYEs. The SUT is now
/// Mortal on the b-leg by its OWN act (it relayed that BYE), and the ban on new
/// in-dialog requests still does not reach the ACK it owes bob.
#[tokio::test]
async fn caller_byes_across_the_reinvite_2xx_and_the_sut_still_acks() {
    let h = Harness::with_transit_delay("b2bua-bye-reinvite-ack-caller", 0);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;

    let (mut alice_dialog, _bob_dialog) = establish(&alice, &bob, &b2bua).await;

    let mut reinv = alice_dialog.reinvite(None).await;
    let reinvite_cseq = alice_dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;

    // ── alice hangs up; the BYE is relayed onto the b-leg ────────────────────
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    let mut bob_bye = bob.receive("BYE").await;

    // ── and only then does she ACK. The SUT, Mortal on the leg it just BYEd,
    //    still emits the b-leg ACK. ────────────────────────────────────────────
    alice_dialog.ack_for(reinvite_cseq, Some(REOFFER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(
        ack.request().cseq().seq(),
        bob_reinv.request().cseq().seq(),
        "§13.2.2.4: the ACK echoes the re-INVITE's CSeq",
    );

    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **The receiving half.** bob re-INVITEs, alice answers 200 — and the SUT ACKs
/// alice on receipt, since that re-INVITE carried the offer. bob then BYEs
/// without ACKing the 200 he was handed, the whole call is reaped, and bob's
/// late ACK arrives. §17.1.1.3 gives an ACK no response at all, so the SUT must
/// absorb it: a 481 here is a response to a request that can never take one,
/// and it would restart the peer's teardown reasoning on a call already gone.
#[tokio::test(start_paused = true)]
async fn a_late_ack_into_a_reaped_dialog_is_absorbed_never_answered() {
    let h = Harness::new("b2bua-bye-reinvite-ack-late");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    // bob withholds the re-INVITE ACK until after his own BYE — the deliberate
    // non-compliance this test exists for (RFC 5407 §3.1.6 / §3.2.4).
    h.allow_violation(
        "unacked-2xx-not-cleared",
        "bob deliberately holds his re-INVITE ACK past his own BYE — the late-ACK cross under test",
    );
    let (_alice_dialog, mut bob_dialog) = establish(&alice, &bob, &b2bua).await;

    // ── bob re-INVITEs, alice answers, bob stays silent ──────────────────────
    let mut reinv = bob_dialog.reinvite(Some(REANSWER)).await;
    let reinvite_cseq = bob_dialog.local_cseq();
    let mut alice_reinv = alice.receive("INVITE").await;
    alice_reinv.respond(200, "OK").with_sdp(REOFFER).await;
    reinv.expect(200).await;
    // alice's 200 answers an offer-carrying re-INVITE, so her own ACK is owed
    // on receipt and goes out before bob has moved.
    alice.receive("ACK").await;

    // ── bob BYEs instead of ACKing; the call tears down and is reaped ────────
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    // ── the late ACK lands on nothing. It must draw no response whatsoever. ──
    bob_dialog.ack_for(reinvite_cseq, None).await;
    // A paused clock, so 50 ms is exact: a wrongly-sent 481 lands within one
    // transit hop, and the nearest legitimate emission toward bob — the
    // §13.3.1.4 re-INVITE-2xx rung at T1 — is 500 ms away.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        bob.drain().await,
        0,
        "§17.1.1.3: a late ACK is absorbed, never answered (a 481 here is a defect)"
    );
    assert_eq!(b2bua.active_calls(), 0, "the late ACK created no call state");
    b2bua.assert_fully_reaped();

    alice.drain().await;
    let _report = h.finish().await;
}

/// **The 2xx repeats while the dialog is dying** — RFC 3261 §13.2.2.4: "The ACK
/// MUST be passed to the client transport every time a retransmission of the
/// 2xx final response that triggered the ACK arrives." Being Mortal changes
/// nothing about that count: the first 200 draws its ACK on receipt, bob BYEs,
/// and the copy he sends after the BYE draws its own on the same transaction.
#[tokio::test]
async fn a_repeated_reinvite_2xx_across_the_bye_draws_one_ack_per_copy() {
    const BOB: &str = "127.0.0.1:5072";
    let h = Harness::with_transit_delay("b2bua-bye-reinvite-ack-repeat", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5072).start(&h, "b2bua", "127.0.0.1:5082").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    let (mut alice_dialog, mut bob_dialog) = establish(&alice, &bob, &b2bua).await;

    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = alice_dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob.receive("ACK").await;

    // ── bob hangs up, then ladders that 200 anyway (his ACK was lost) ────────
    let mut bob_bye = bob_dialog.bye().await;
    bob_bye.expect(200).await;
    let mut alice_bye = alice.receive("BYE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;

    // ── the Mortal leg owes the repeat its own ACK ───────────────────────────
    bob.receive("ACK").await;

    // ── alice's ACK arrives last and is hop-local ────────────────────────────
    alice_dialog.ack_for(reinvite_cseq, None).await;

    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    // The re-INVITE's two ACKs are ONE client transaction: a fresh branch per
    // copy would never quiesce bob's re-INVITE server txn.
    let reinvite_acks: Vec<Vec<u8>> = acks_to(&report, b2bua.addr, bob_addr)
        .into_iter()
        .filter(|raw| sip_message::sniff::cseq_number(raw) == Some(reinvite_cseq))
        .collect();
    assert_eq!(reinvite_acks.len(), 2, "one ACK per re-INVITE 2xx received (§13.2.2.4)");
    assert_eq!(
        reinvite_acks[0], reinvite_acks[1],
        "the re-ACK for a retransmitted re-INVITE 2xx is the SAME ACK",
    );
}

/// INVITE → 180 → 200 → ACK on both legs; returns the confirmed dialogs. bob's
/// ACK is the SUT's own, owed on receipt of his 200; alice's follows and is
/// absorbed.
async fn establish(
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    b2bua: &B2buaSut,
) -> (scenario_harness::Dialog, scenario_harness::Dialog) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    bob.receive("ACK").await;
    let alice_dialog = call.ack().await;
    let bob_dialog = uas.dialog();
    assert_eq!(b2bua.active_calls(), 1, "call established");
    (alice_dialog, bob_dialog)
}
