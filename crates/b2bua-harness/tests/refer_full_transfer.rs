//! REFER full-transfer scenarios (slice 5c). Port of
//! `tests/scenarios/refer-full-transfer.ts`.
//!
//! Drive the complete A↔B↔C transfer through both re-INVITEs to completion
//! (`merge(a, c)`) or to rollback on any failure in the a-realign phase. Slices
//! 5a+5b established everything up to the a-realign re-INVITE toward A; this
//! file exercises A's RESPONSE to that re-INVITE and the merge/teardown:
//!
//!   1. FullHappy        — A 200 → ACK A, merge(a, c), then A BYE tears down B+C.
//!   2. FullARejectRealign — A 488 → rollback BYEs all three legs.
//!   3. FullAGlareReinvite — A glares (re-INVITE) mid a-realigning → 491, resume.
//!   4. FullABye         — A BYEs before answering → relay-bye begin-termination
//!                          BYEs the orphaned B + C.
//!   5. FullATimeout     — A never answers (32s watchdog) → rollback BYEs all 3.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// C's active answer to the c-realign re-INVITE (sendrecv on C's real port). The
// a-realign re-INVITE toward A must carry *this* body (the one-way-audio guard).
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// A's answer to the a-realign re-INVITE (A accepts C's media).
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5667;

fn x_api_allow_c(extras: &str) -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}{extras}}}"#
    )
}

fn refer_to_charlie() -> String {
    format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>")
}

/// Assert a received NOTIFY carries `Event: refer`, a `Subscription-State`
/// starting with `prefix`, and a sipfrag body containing `frag`.
fn assert_notify(txn: &ServerTxn, prefix: &str, frag: &str) {
    let req = txn.request();
    assert_eq!(req.method, "NOTIFY", "expected NOTIFY");
    assert_eq!(get_header(&req.headers, "event").unwrap_or(""), "refer", "NOTIFY Event: refer");
    let ss = get_header(&req.headers, "subscription-state").unwrap_or("");
    assert!(ss.starts_with(prefix), "subscription-state {ss:?} should start with {prefix:?}");
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains(frag), "sipfrag body {body:?} should contain {frag:?}");
}

/// Assert a re-INVITE carries `body`, CSeq > 1, and a Contact carrying `leg=<leg>`.
fn assert_reinvite(req: &sip_message::SipRequest, body: &str, leg: &str) {
    assert_eq!(req.method, "INVITE", "expected re-INVITE");
    assert_eq!(String::from_utf8_lossy(&req.body), body, "re-INVITE body should equal expected SDP");
    assert!(req.cseq.seq > 1, "re-INVITE CSeq.seq {} should be > 1", req.cseq.seq);
    let contact = get_header(&req.headers, "contact").unwrap_or("");
    assert!(contact.contains(&format!("leg={leg}")), "Contact {contact:?} should carry leg={leg}");
}

// ── 1. Full happy transfer → merge(a, c) ─────────────────────────────────

#[tokio::test]
async fn refer_allow_full_happy() {
    let h = Harness::with_transit_delay("refer-allow-full-happy", 1);
    let alice = h.agent("alice", "127.0.0.1:5961").await;
    let bob = h.agent("bob", "127.0.0.1:5971").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5981", "127.0.0.1", 5971).await;

    // A↔B established.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // REFER → 202.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(""))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // C answers initial INVITE 200; B2BUA ACKs.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // c-realign re-INVITE C with A's SDP; C answers its active sendrecv answer.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // a-realign re-INVITE A carries C's active answer, Contact leg=a.
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");
    // A answers 200 → B2BUA ACKs A and merges a↔c (transfer complete).
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    // A↔C now bridged. A BYE → relay-bye 200 + begin-termination BYEs the now
    // peered C and the orphaned B leg (proves the merge: A's hangup reaches C).
    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 2. A rejects a-realign re-INVITE (488) → rollback ────────────────────

#[tokio::test]
async fn refer_allow_full_a_reject_realign() {
    let h = Harness::with_transit_delay("refer-allow-full-a-reject-realign", 1);
    let alice = h.agent("alice", "127.0.0.1:5962").await;
    let bob = h.agent("bob", "127.0.0.1:5972").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5982", "127.0.0.1", 5972).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(""))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // A rejects the a-realign re-INVITE 488; the UAC txn layer auto-ACKs.
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");
    a_realign.respond(488, "Not Acceptable Here").await;
    // B2BUA (the re-INVITE UAC) ACKs A's 488.
    alice.receive("ACK").await;

    // Rollback — begin-termination BYEs alice, bob and charlie.
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}

// ── 3. A glares (re-INVITE) during a-realigning → 491, then resume ───────

#[tokio::test]
async fn refer_allow_full_a_glare_reinvite() {
    let h = Harness::with_transit_delay("refer-allow-full-a-glare-reinvite", 1);
    let alice = h.agent("alice", "127.0.0.1:5963").await;
    let bob = h.agent("bob", "127.0.0.1:5973").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5983", "127.0.0.1", 5973).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(""))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // Observe the a-realign re-INVITE before answering.
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    // A glares — fires its own re-INVITE mid a-realigning. B2BUA replies 491.
    let mut a_glare = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(ALICE_REALIGN_ANSWER)
        .send()
        .await;
    a_glare.expect(491).await;

    // A completes the original a-realign → ACK A, merge a↔c.
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    // Teardown — A BYE → BYEs C (now peered) and the orphaned B.
    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 4. A BYEs while its a-realign re-INVITE is outstanding → teardown ────
//
// skipFinalSweep (TS refer-full-transfer.ts:361): the TS harness 24h end-of-
// scenario sweep races A's outstanding a-realign re-INVITE retransmits. The
// Rust harness has no such sweep — we drive the BYE explicitly between advances
// and tolerate the CANCEL the B2BUA may emit for the in-flight re-INVITE; the
// assertion that B+C are BYE'd is NOT relaxed. A BYE during a-realigning rides
// the CORE `relay-bye` path: 200 to A, then begin-termination BYEs B (orphan)
// and C — no dedicated transfer rule is involved.

#[tokio::test]
async fn refer_allow_full_a_bye_during_a_realign() {
    let h = Harness::with_transit_delay("refer-allow-full-a-bye-during-a-realign", 1);
    let alice = h.agent("alice", "127.0.0.1:5964").await;
    let bob = h.agent("bob", "127.0.0.1:5974").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5984", "127.0.0.1", 5974).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(""))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // Observe the a-realign re-INVITE but A hangs up instead of answering.
    let mut a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;

    // B2BUA tears down bob and charlie (the orphaned legs).
    bob.receive("BYE").await.respond(200, "OK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;

    let _ = &mut a_realign;
    let _ = h.finish().await;
}

// ── 5. A never answers the a-realign re-INVITE → 32s watchdog rollback ───
//
// skipFinalSweep (TS refer-full-transfer.ts:442): the TS 24h sweep races A's
// outstanding a-realign re-INVITE retransmits. The Rust harness has no sweep —
// we drive the exact 32s `refer_reinvite_answer` deadline and tolerate the
// INVITE/CANCEL/BYE retransmits the crossed deadline emits (the watchdog rule
// and the INVITE Timer B can both fire near 32s). The assertion that all three
// legs are BYE'd is NOT relaxed.

#[tokio::test(start_paused = true)]
async fn refer_allow_full_a_reinvite_timeout() {
    let h = Harness::new("refer-allow-full-a-reinvite-timeout");
    let alice = h.agent("alice", "127.0.0.1:5965").await;
    let bob = h.agent("bob", "127.0.0.1:5975").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5985", "127.0.0.1", 5975).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(""))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // Observe the a-realign re-INVITE arrival but never answer it.
    let a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    // Advance past the 32s refer_reinvite_answer watchdog → rollback.
    h.advance(Duration::from_secs(33)).await;

    // begin-termination BYEs alice + bob + charlie. Tolerate the re-INVITE /
    // CANCEL / BYE retransmits the crossed deadline emits.
    alice
        .receive_tolerating("BYE", &["INVITE", "CANCEL", "OPTIONS"])
        .await
        .respond(200, "OK")
        .await;
    bob.receive_tolerating("BYE", &["INVITE", "CANCEL", "OPTIONS"])
        .await
        .respond(200, "OK")
        .await;
    charlie
        .receive_tolerating("BYE", &["INVITE", "CANCEL", "OPTIONS"])
        .await
        .respond(200, "OK")
        .await;

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}
