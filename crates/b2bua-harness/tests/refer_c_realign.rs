//! REFER c-realigning scenarios (slice 5b). Port of
//! `tests/scenarios/refer-c-realign.ts`.
//!
//! Build on the slice-5a C-leg lifecycle by adding the B2BUA-originated
//! re-INVITE to C carrying A's SDP (the c-realign re-INVITE). The charlie UA
//! therefore plays a UAS role for that re-INVITE. Once C answers 200 and the
//! B2BUA ACKs, the phase advances to `a-realigning` and the B2BUA fires the
//! a-realign re-INVITE toward A. Slice 5b STOPS once that a-realign INVITE
//! lands (verifying its body is C's active answer and Contact carries `leg=a`);
//! the a-realign 200 / merge live in slice 5c (`refer-full-transfer.ts`).
//!
//! The rollback cases (CReject488, CTimeout) drive `begin-termination`, which
//! BYEs all three confirmed legs (alice, bob, charlie).

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// C's active answer to the c-realign re-INVITE (sendrecv on C's real port). The
// a-realign re-INVITE toward A must carry *this* body (not the initial held
// answer) — the one-way-audio guard (slice5-refer-design.md §4).
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5667;

/// The `X-Api-Call` instruction JSON authorizing a transfer to charlie.
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
    let event = get_header(&req.headers, "event").unwrap_or("");
    assert_eq!(event, "refer", "NOTIFY Event: refer");
    let ss = get_header(&req.headers, "subscription-state").unwrap_or("");
    assert!(
        ss.starts_with(prefix),
        "subscription-state {ss:?} should start with {prefix:?}"
    );
    let body = String::from_utf8_lossy(&req.body);
    assert!(
        body.contains(frag),
        "sipfrag body {body:?} should contain {frag:?}"
    );
}

/// Assert a re-INVITE carries `body`, an INVITE CSeq > 1 (a re-INVITE, not the
/// initial INVITE), and a Contact whose URI carries `leg=<leg>`.
fn assert_reinvite(req: &sip_message::SipRequest, body: &str, leg: &str) {
    assert_eq!(req.method, "INVITE", "expected re-INVITE");
    assert_eq!(
        String::from_utf8_lossy(&req.body),
        body,
        "re-INVITE body should equal expected SDP"
    );
    assert!(req.cseq.seq > 1, "re-INVITE CSeq.seq {} should be > 1", req.cseq.seq);
    let contact = get_header(&req.headers, "contact").unwrap_or("");
    assert!(
        contact.contains(&format!("leg={leg}")),
        "Contact {contact:?} should carry leg={leg}"
    );
}

// ── 1. Happy c-realign — re-INVITE C answered 200 → phase a-realigning ──

#[tokio::test]
async fn refer_allow_c_realign_happy() {
    let h = Harness::with_transit_delay("refer-allow-c-realign-happy", 1);
    let alice = h.agent("alice", "127.0.0.1:5931").await;
    let bob = h.agent("bob", "127.0.0.1:5941").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5951", "127.0.0.1", 5941).await;

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

    // NOTIFY 100 active.
    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // C receives initial INVITE (held SDP), rings.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;

    // NOTIFY 180 active.
    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180 Ringing");
    n180.respond(200, "OK").await;

    // C answers initial INVITE 200; B2BUA ACKs (it is C's UAC).
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    // NOTIFY 200 terminated (final).
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // B2BUA re-INVITEs C with A's exact SDP, CSeq bumped, Contact tagged leg=b-2.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    // C answers the c-realign re-INVITE with its active sendrecv answer.
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // Phase → a-realigning: B2BUA re-INVITEs A with C's active answer, Contact
    // leg=a. Slice 5b stops here (the a-realign 200 / merge are slice 5c).
    let a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}

// ── 2. C rejects c-realign re-INVITE (488) → rollback ────────────────────

#[tokio::test]
async fn refer_allow_c_realign_c_reject_488() {
    let h = Harness::with_transit_delay("refer-allow-c-realign-c-reject-488", 1);
    let alice = h.agent("alice", "127.0.0.1:5932").await;
    let bob = h.agent("bob", "127.0.0.1:5942").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5952", "127.0.0.1", 5942).await;

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

    // C rejects the c-realign re-INVITE with 488; the UAC txn layer auto-ACKs.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");
    c_realign.respond(488, "Not Acceptable Here").await;
    charlie.receive("ACK").await;

    // Rollback — begin-termination BYEs alice, bob (b-1) and charlie (b-2).
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}

// ── 3. C does not answer the c-realign re-INVITE → 32s watchdog rollback ──
//
// skipFinalSweep (TS refer-c-realign.ts:298): the pending c-realign re-INVITE
// Timer B (≈32s, the INVITE UAC transaction timeout) fires alongside the
// rule-level `refer_reinvite_answer` watchdog (32s); under a paused clock the
// single advance that crosses 32s trips both, and the three BYE 200 round-trips
// race the re-INVITE/Timer-B retransmits. We drive the exact deadline and
// tolerate the resulting INVITE/CANCEL/BYE retransmits (the harness equivalent
// of the TS `allowExtra` + `.skipFinalSweep()`); the assertion that all three
// legs are BYE'd is NOT relaxed.

#[tokio::test(start_paused = true)]
async fn refer_allow_c_realign_c_timeout() {
    let h = Harness::new("refer-allow-c-realign-c-timeout");
    let alice = h.agent("alice", "127.0.0.1:5933").await;
    let bob = h.agent("bob", "127.0.0.1:5943").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5953", "127.0.0.1", 5943).await;

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

    // Observe the c-realign re-INVITE arrival but never answer it.
    let c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");

    // Advance past the 32s refer_reinvite_answer watchdog → rollback.
    h.advance(Duration::from_secs(33)).await;

    // begin-termination BYEs alice + bob + charlie (all confirmed). Tolerate the
    // re-INVITE / CANCEL / BYE retransmits the crossed deadline emits.
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

// ── 4. C glare re-INVITE during c-realigning → 491, then resume ──────────

#[tokio::test]
async fn refer_allow_c_realign_c_glare() {
    let h = Harness::with_transit_delay("refer-allow-c-realign-c-glare", 1);
    let alice = h.agent("alice", "127.0.0.1:5934").await;
    let bob = h.agent("bob", "127.0.0.1:5944").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5954", "127.0.0.1", 5944).await;

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
    let mut charlie_dialog = charlie_uas.dialog();

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Receive the B2BUA's c-realign re-INVITE so we are firmly in c-realigning.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");

    // Before answering, C fires its own re-INVITE — glare. B2BUA replies 491.
    let mut c_glare = charlie_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(CHARLIE_ACTIVE_ANSWER)
        .send()
        .await;
    c_glare.expect(491).await;

    // Resume the B2BUA-originated c-realign exchange.
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    // Phase → a-realigning: B2BUA re-INVITEs A with C's active answer, leg=a.
    let a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}

// ── 5. B-leg non-BYE during c-realigning → 481, then resume ─────────────

#[tokio::test]
async fn refer_allow_c_realign_b_non_bye() {
    let h = Harness::with_transit_delay("refer-allow-c-realign-b-non-bye", 1);
    let alice = h.agent("alice", "127.0.0.1:5935").await;
    let bob = h.agent("bob", "127.0.0.1:5945").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5955", "127.0.0.1", 5945).await;

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

    // Receive the B2BUA's c-realign re-INVITE so we are firmly in c-realigning.
    let mut c_realign = charlie.receive("INVITE").await;
    assert_reinvite(c_realign.request(), OFFER, "b-2");

    // B sends an in-dialog INFO during c-realigning → rejected 481.
    let mut b_info = bob_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/dtmf-relay")
        .send()
        .await;
    b_info.expect(481).await;

    // Finish the c-realign exchange — C 200 → ACK → phase a-realigning.
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;

    let a_realign = alice.receive("INVITE").await;
    assert_reinvite(a_realign.request(), CHARLIE_ACTIVE_ANSWER, "a");

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}
