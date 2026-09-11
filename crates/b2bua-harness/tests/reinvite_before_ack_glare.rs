//! Same-direction re-INVITE glare: a second in-dialog INVITE that overtakes the
//! ACK for the first one's 2xx is rejected 491 Request Pending.
//!
//! RFC 3261 §14.1: a new INVITE waits for the prior INVITE **server**
//! transaction to reach `Confirmed` — the ACK, not merely the final (RFC 6026
//! calls the interval `Accepted`). Relaying the newcomer instead would make the
//! B2BUA emit its own overtaking INVITE toward the peer face, where the prior
//! 2xx is likewise still un-ACKed. `reinvite-glare`'s un-ACKed-2xx arm
//! (`pending_reinvite_2xx` on the source dialog OR the relay-target dialog)
//! answers the newcomer 491 locally, on BOTH faces.
//!
//! `reinvite.rs::crossing_reinvite_glare` pins the OTHER glare shape — two
//! peers re-INVITEing at once — caught by the inbound pending request on the
//! source dialog.

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";
const REOFFER2: &str = "v=0\r\no=bob 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";
const LATE_OFFER: &str = "v=0\r\no=alice 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30003 RTP/AVP 0\r\n";
const LATE_ANSWER: &str = "v=0\r\no=bob 4 4 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30004 RTP/AVP 0\r\n";

/// Bob holds the ACK for his first re-INVITE's 2xx and re-INVITEs again — the
/// peer behaviour a corpus capture exhibits (the second INVITE overtakes the
/// ACK by ~190 ms). The B2BUA must answer the newcomer 491 locally and leave
/// the first renegotiation intact; bob then ACKs it and the delayed-offer
/// re-INVITE that follows completes normally.
#[tokio::test]
async fn reinvite_overtaking_the_ack_is_491ed() {
    let h = Harness::with_transit_delay("b2bua-reinvite-before-ack", 0);
    // Bob's overtaking re-INVITE is the corner case under test: he is the buggy
    // peer here, and the deviation stays on the peer side — the SUT's own output
    // is what the 491 assertion pins.
    h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "bob deliberately re-INVITEs before ACKing the prior 2xx — the peer \
         misbehaviour this test exists to interwork with",
    );
    let alice = h.agent("alice", "127.0.0.1:5049").await;
    let bob = h.agent("bob", "127.0.0.1:5059").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5059).start(&h, "b2bua", "127.0.0.1:5099").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // ── re-INVITE #1 is answered … and bob sits on the ACK ──
    let mut reinv1 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let cseq1 = reinv1.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv1.expect(200).await;

    // ── re-INVITE #2 overtakes that ACK → 491, answered locally ──
    let mut reinv2 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    reinv2.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── bob ACKs #1; the first renegotiation was never disturbed ──
    bob_dialog.ack_for(cseq1, None).await;
    alice.receive("ACK").await;

    // ── the dialog still renegotiates: the delayed-offer re-INVITE completes ──
    let mut reinv3 = bob_dialog.request(InDialogMethod::Invite, None).await;
    let mut alice_uas3 = alice.receive("INVITE").await;
    assert!(alice_uas3.request().body().is_empty(), "offerless re-INVITE stays offerless");
    alice_uas3.respond(200, "OK").with_sdp(LATE_OFFER).await;
    reinv3.expect(200).await;
    bob_dialog.ack(Some(LATE_ANSWER)).await;
    alice.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}

/// The same overtaking shape from the CALLER face: alice holds the ACK for her
/// first re-INVITE's 2xx and re-INVITEs again. The un-ACKed-2xx state was
/// already tracked on the a-leg dialog (`pending_reinvite_2xx`); this pins the
/// glare arm reading it there.
#[tokio::test]
async fn caller_reinvite_overtaking_the_ack_is_491ed() {
    let h = Harness::with_transit_delay("b2bua-reinvite-before-ack-a", 0);
    h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "alice deliberately re-INVITEs before ACKing the prior 2xx — the peer \
         misbehaviour this test exists to interwork with",
    );
    let alice = h.agent("alice", "127.0.0.1:5041").await;
    let bob = h.agent("bob", "127.0.0.1:5051").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5051).start(&h, "b2bua", "127.0.0.1:5095").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── re-INVITE #1 is answered … and alice sits on the ACK ──
    let mut reinv1 = alice_dialog.request(InDialogMethod::Invite, Some(REANSWER)).await;
    let cseq1 = reinv1.sent_invite().expect("the re-INVITE alice just sent").cseq().seq();
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(REOFFER).await;
    reinv1.expect(200).await;

    // ── re-INVITE #2 overtakes that ACK → 491, answered locally ──
    let mut reinv2 = alice_dialog.request(InDialogMethod::Invite, Some(LATE_OFFER)).await;
    reinv2.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── alice ACKs #1; the first renegotiation was never disturbed ──
    alice_dialog.ack_for(cseq1, None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}

/// The emission-side arm: while bob's re-INVITE 2xx is still un-ACKed, ALICE
/// re-INVITEs. The relay target (bob's dialog) holds the un-ACKed 2xx, so the
/// B2BUA must not emit an overtaking INVITE toward bob (RFC 3261 §14.1 rule 2
/// applied to its own UAC face) — alice's newcomer is 491'd locally and bob
/// never sees a second INVITE. After bob's ACK the dialog renegotiates
/// normally.
#[tokio::test]
async fn reinvite_toward_the_unacked_face_is_491ed() {
    let h = Harness::with_transit_delay("b2bua-reinvite-before-ack-x", 0);
    // Alice's re-INVITE lands while her own 2xx to bob's re-INVITE is not yet
    // confirmed (bob's ACK has not been relayed) — the racing-peer shape under
    // test; the waiver is conditional and the SUT's own output stays compliant.
    h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "alice re-INVITEs while the prior renegotiation is still un-ACKed — \
         the racing-peer shape this test exists to interwork with",
    );
    let alice = h.agent("alice", "127.0.0.1:5042").await;
    let bob = h.agent("bob", "127.0.0.1:5052").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5052).start(&h, "b2bua", "127.0.0.1:5096").await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // ── bob's re-INVITE is answered … and bob sits on the ACK ──
    let mut reinv1 = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let cseq1 = reinv1.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv1.expect(200).await;
    // bob's re-INVITE carried the offer, so the ACK for alice's 200 owes no body
    // and the UAC core composes it on receipt (§13.2.2.4) — it reaches her before
    // bob has moved. What holds the face un-ACKed is bob's own silence.
    alice.receive("ACK").await;

    // ── alice re-INVITEs into the un-ACKed face → 491, bob sees nothing ──
    let mut alice_reinv = alice_dialog.request(InDialogMethod::Invite, Some(LATE_OFFER)).await;
    alice_reinv.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── bob ACKs #1; the first renegotiation was never disturbed ──
    // Hop-local: alice's face was ACKed on receipt, so nothing is owed onward.
    bob_dialog.ack_for(cseq1, None).await;

    // ── alice retries and the renegotiation completes ──
    let mut reinv3 = alice_dialog.request(InDialogMethod::Invite, Some(LATE_OFFER)).await;
    let mut bob_uas3 = bob.receive("INVITE").await;
    assert!(!bob_uas3.request().body().is_empty(), "alice's retried offer reaches bob");
    bob_uas3.respond(200, "OK").with_sdp(LATE_ANSWER).await;
    reinv3.expect(200).await;
    alice_dialog.ack(None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}
