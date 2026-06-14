//! REFER allow-path scenarios (slice 5a). Port of `tests/scenarios/refer-allow.ts`.
//!
//! Each scenario establishes an A↔B call and issues a REFER from B that the
//! scripted `/call/refer` authorizes (`X-Api-Call` `refer-allow-c`). The B2BUA
//! builds a C leg with held SDP and drives it through the initial
//! INVITE/200/ACK — stopping **before** the c-realign re-INVITE (slice 5b),
//! which the scenarios tolerate as an extra INVITE toward C.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

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

// ── 1. Happy path up to final NOTIFY (no realign driven) ──────────────────

#[tokio::test]
async fn refer_allow_happy() {
    let h = Harness::with_transit_delay("refer-allow-happy", 1);
    let alice = h.agent("alice", "127.0.0.1:5901").await;
    let bob = h.agent("bob", "127.0.0.1:5911").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5911).start(&h, "b2bua", "127.0.0.1:5921").await;

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

    // Charlie receives the C INVITE (held SDP), rings.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;

    // NOTIFY 180 active.
    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180 Ringing");
    n180.respond(200, "OK").await;

    // Charlie answers 200; B2BUA ACKs (it is C's UAC).
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    // NOTIFY 200 terminated — subscription closes.
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Slice 5b: c-realign re-INVITE toward C — receive it (don't reply) so the
    // dialog tracker stays in sync for the BYE.
    let mut charlie_dialog = charlie_uas.dialog();
    charlie.receive("INVITE").await;

    // A↔B still bridged; tear down via A BYE → BYEs every confirmed leg (B + C).
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    let _ = &mut charlie_dialog;

    let _ = h.finish().await;
}

// ── 2. C rejects 486 Busy Here → NOTIFY 486 terminated ────────────────────

#[tokio::test]
async fn refer_allow_c486() {
    let h = Harness::with_transit_delay("refer-allow-c-rejects-486", 1);
    let alice = h.agent("alice", "127.0.0.1:5902").await;
    let bob = h.agent("bob", "127.0.0.1:5912").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5912).start(&h, "b2bua", "127.0.0.1:5922").await;

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

    // Charlie rejects 486. The txn layer (C's UAC) auto-ACKs the non-2xx.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(486, "Busy Here").await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 486");
    nterm.respond(200, "OK").await;

    // A↔B remains alive — tear down normally (C is gone).
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 5. Multiple 18x dedup: 180 → 183 → 180 → exactly 3 NOTIFYs ────────────

#[tokio::test]
async fn refer_allow_c_multiple_18x() {
    let h = Harness::with_transit_delay("refer-allow-c-multiple-18x", 1);
    let alice = h.agent("alice", "127.0.0.1:5905").await;
    let bob = h.agent("bob", "127.0.0.1:5915").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5915).start(&h, "b2bua", "127.0.0.1:5925").await;

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

    // 180 #1 → NOTIFY 180.
    charlie_uas.respond(180, "Ringing").await;
    let mut n180a = bob.receive("NOTIFY").await;
    assert_notify(&n180a, "active", "SIP/2.0 180");
    n180a.respond(200, "OK").await;

    // 183 #1 → NOTIFY 183 (183 clears the 180 dedup).
    charlie_uas.respond(183, "Session Progress").await;
    let mut n183 = bob.receive("NOTIFY").await;
    assert_notify(&n183, "active", "SIP/2.0 183");
    n183.respond(200, "OK").await;

    // 180 #2 → NOTIFY 180 again (dedup is against the last status only).
    charlie_uas.respond(180, "Ringing").await;
    let mut n180b = bob.receive("NOTIFY").await;
    assert_notify(&n180b, "active", "SIP/2.0 180");
    n180b.respond(200, "OK").await;

    // Identical 180 once more — deduped, no NOTIFY.
    charlie_uas.respond(180, "Ringing").await;

    // Finalise with 200 → terminated NOTIFY. The deduped 180 retransmits race
    // the 200's NOTIFY; tolerate the extra 180 NOTIFY.
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    // The deduped trailing 180 emits no NOTIFY, so the next NOTIFY is the
    // terminated 200 sipfrag.
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Slice 5b: c-realign re-INVITE toward C — receive (don't reply).
    let mut charlie_dialog = charlie_uas.dialog();
    charlie.receive("INVITE").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    let _ = &mut charlie_dialog;

    let _ = h.finish().await;
}

// ── 4. C no-answer → NOTIFY 408 terminated after the no-answer timer ──────

#[tokio::test(start_paused = true)]
async fn refer_allow_c_no_answer() {
    let h = Harness::new("refer-allow-c-no-answer");
    let alice = h.agent("alice", "127.0.0.1:5904").await;
    let bob = h.agent("bob", "127.0.0.1:5914").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5914).start(&h, "b2bua", "127.0.0.1:5924").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // Short 5s no-answer timeout so the test runs fast under virtual time.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c(r#","no_answer_timeout_sec":5"#))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // Charlie rings but never answers.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(180, "Ringing").await;

    let mut n180 = bob.receive("NOTIFY").await;
    assert_notify(&n180, "active", "SIP/2.0 180 Ringing");
    n180.respond(200, "OK").await;

    // Advance past the 5s no-answer timer → NOTIFY 408 terminated. The advance
    // crosses the deadline, so the 408 NOTIFY's 200 round-trip races the txn
    // retransmit; tolerate the duplicate NOTIFYs (TS `bob.allowExtra("NOTIFY")`).
    h.advance(Duration::from_secs(6)).await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 408");
    nterm.respond(200, "OK").await;

    // A↔B survives; tear down normally. C was destroyed (CANCEL toward the
    // still-early C leg, no BYE). Drain any retransmitted NOTIFY before the BYE.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY"]).await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 3. C rejects 603 Declined → NOTIFY 603 terminated ─────────────────────

#[tokio::test]
async fn refer_allow_c603() {
    let h = Harness::with_transit_delay("refer-allow-c-rejects-603", 1);
    let alice = h.agent("alice", "127.0.0.1:5903").await;
    let bob = h.agent("bob", "127.0.0.1:5913").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5913).start(&h, "b2bua", "127.0.0.1:5923").await;

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
    charlie_uas.respond(603, "Declined").await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 603");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}
