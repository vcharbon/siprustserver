//! Same-direction re-INVITE glare: a second in-dialog INVITE that overtakes the
//! ACK for the first one's 2xx is rejected 491 Request Pending.
//!
//! RFC 3261 §14.1: a new INVITE waits for the prior INVITE **server**
//! transaction to reach `Confirmed` — the ACK, not merely the final (RFC 6026
//! calls the interval `Accepted`). Relaying the newcomer instead would make the
//! B2BUA emit its own overtaking INVITE toward the peer face, where the prior
//! 2xx is likewise still un-ACKed, AND reset that face's ACK obligation, so the
//! answerer's 2xx would never be acknowledged at all. `reinvite-glare`'s
//! un-ACKed-2xx arm reads every mark of the interval — a 2xx this stack sent
//! awaiting the peer's ACK, and a 2xx it took whose relayed ACK has not left —
//! on the source dialog OR the relay-target dialog, and answers the newcomer
//! 491 locally on BOTH faces. The call's own answer counts like a
//! renegotiation's: the interval is the same one.
//!
//! `reinvite.rs::crossing_reinvite_glare` pins the OTHER glare shape — two
//! peers re-INVITEing at once — caught by the inbound pending request on the
//! source dialog.

use std::net::SocketAddr;

use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, WaiverScope};
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
    h.waive(
        WaiverScope::rule(
            "no-re-invite-while-invite-in-progress",
            "bob deliberately re-INVITEs before ACKing the prior 2xx — the peer \
             misbehaviour this test exists to interwork with",
        )
        .on_party("bob"),
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
    h.waive(
        WaiverScope::rule(
            "no-re-invite-while-invite-in-progress",
            "alice deliberately re-INVITEs before ACKing the prior 2xx — the peer \
             misbehaviour this test exists to interwork with",
        )
        .on_party("alice"),
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
    // test. The audit's §14.1 rule charges a party that overtakes an INVITE IT
    // sent, and alice sent none here, so nothing is waived.
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

    // ── alice re-INVITEs into the un-ACKed face → 491, bob sees nothing ──
    let mut alice_reinv = alice_dialog.request(InDialogMethod::Invite, Some(LATE_OFFER)).await;
    alice_reinv.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── bob ACKs #1; the first renegotiation was never disturbed ──
    // That ACK is what alice gets: it is the one the 2xx owes (§13.2.2.4).
    bob_dialog.ack_for(cseq1, None).await;
    alice.receive("ACK").await;

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

/// **The call's own answer.** alice re-INVITEs before ACKing the INITIAL 2xx,
/// so her own INVITE server transaction is still `Accepted` on the a-leg and
/// the b-leg's 2xx has no ACK yet. The newcomer is 491'd locally and never
/// reaches the callee — relaying it would reset the b-leg's ACK obligation and
/// leave bob's 200 unacknowledged for good. Her late ACK then relays (exactly
/// once), and the §14.1 retry renegotiates normally.
#[tokio::test]
async fn caller_reinvite_overtaking_the_initial_ack_is_491ed() {
    const BOB: &str = "127.0.0.1:5053";
    let h = Harness::with_transit_delay("b2bua-reinvite-before-initial-ack-a", 0);
    // Alice's overtaking re-INVITE is the corner case: she is the buggy peer,
    // and the deviation stays on the peer side.
    h.waive(
        WaiverScope::rule(
            "no-re-invite-while-invite-in-progress",
            "alice deliberately re-INVITEs before ACKing the 2xx that answered her own \
             INVITE — the overtaking peer this test exists to interwork with",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5043").await;
    let bob = h.agent("bob", BOB).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5053).start(&h, "b2bua", "127.0.0.1:5097").await;
    let bob_addr: SocketAddr = BOB.parse().unwrap();

    // ── call answered; alice sits on her ACK ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let initial_cseq = call.invite_cseq();

    // ── her re-INVITE overtakes that ACK → 491, answered locally ──
    let mut early = call.send_request(InDialogMethod::Invite).with_sdp(REOFFER2).send().await;
    early.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── the late ACK relays; bob's 2xx is acknowledged exactly once ──
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── the §14.1 retry renegotiates normally ──
    let mut retry = alice_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    let retry_cseq = alice_dialog.local_cseq();
    let mut bob_uas = bob.receive("INVITE").await;
    assert!(!bob_uas.request().body().is_empty(), "alice's retried offer reaches bob");
    bob_uas.respond(200, "OK").with_sdp(LATE_ANSWER).await;
    retry.expect(200).await;
    alice_dialog.ack_for(retry_cseq, None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    b2bua.assert_fully_reaped();

    // The 491'd newcomer left no trace on the b-leg: two INVITEs (the setup and
    // the retry), and one ACK per 2xx received.
    let to_bob = |prefix: &[u8]| {
        report
            .entries()
            .iter()
            .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(prefix))
            .count()
    };
    assert_eq!(to_bob(b"INVITE "), 2, "the setup and the retry — never the 491'd newcomer");
    assert_eq!(to_bob(b"ACK "), 2, "one ACK per 2xx the callee sent");
    assert_ne!(initial_cseq, retry_cseq, "the retry spends a CSeq of its own");
}

/// **The mirrored case, on the callee face.** bob re-INVITEs before ACKing the
/// initial 2xx this stack answered him with, so the b-leg dialog still holds
/// that un-ACKed answer. The newcomer is 491'd on the b-leg and alice never
/// sees a second INVITE; alice's ACK then relays and bob's retry goes through.
#[tokio::test]
async fn callee_reinvite_before_the_initial_ack_is_491ed() {
    const ALICE: &str = "127.0.0.1:5044";
    let h = Harness::with_transit_delay("b2bua-reinvite-before-initial-ack-b", 0);
    // Bob's re-INVITE lands while the 2xx he sent is still un-ACKed. The audit's
    // §14.1 rule charges a party that overtakes an INVITE IT sent, and bob sent
    // none here, so nothing is waived: the SUT's 491 is the whole subject.
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", "127.0.0.1:5054").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5054).start(&h, "b2bua", "127.0.0.1:5098").await;
    let alice_addr: SocketAddr = ALICE.parse().unwrap();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut bob_dialog = uas.dialog();

    // ── bob re-INVITEs while his own 200 is un-ACKed → 491 ──
    let mut early = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    early.expect(491).await;
    assert_eq!(b2bua.active_calls(), 1, "a 491'd re-INVITE must not disturb the call");

    // ── alice's ACK relays and quiesces bob's INVITE server transaction ──
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── bob's §14.1 retry now relays to alice and completes ──
    let mut retry = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER2)).await;
    let retry_cseq = retry.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    assert!(!alice_uas.request().body().is_empty(), "bob's retried offer reaches alice");
    alice_uas.respond(200, "OK").with_sdp(LATE_ANSWER).await;
    retry.expect(200).await;
    bob_dialog.ack_for(retry_cseq, None).await;
    alice.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    b2bua.assert_fully_reaped();

    // Alice saw exactly one in-dialog INVITE — bob's retry, never the 491'd one.
    let relayed_invites = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == alice_addr && e.raw.starts_with(b"INVITE "))
        .count();
    assert_eq!(relayed_invites, 1, "only the retry crossed the bridge");
}
