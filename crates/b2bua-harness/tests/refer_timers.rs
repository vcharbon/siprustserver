//! REFER safety-timer scenario (slice 5f). Port of
//! `tests/scenarios/refer-timers.ts`.
//!
//! Exercises the `refer_overall_safety` watchdog (the cross-phase end-to-end
//! safety net). The per-scenario config pushes `refer_reinvite_answer` out to
//! 600s and pulls `refer_overall_safety` in to 10s, so when the c-realign
//! re-INVITE toward C goes unanswered the OVERALL watchdog (not the per-phase
//! reinvite-answer timer) trips first → `transfer-overall-timeout` →
//! begin-termination BYEs all three confirmed legs (A, B, C).
//!
//! skipFinalSweep (TS refer-timers.ts:118): the TS 24h end-of-scenario sweep
//! races the pending c-realign re-INVITE's Timer B against the three BYE 200s.
//! The Rust harness has no such sweep — we drive the exact 10s overall-safety
//! deadline and tolerate the INVITE/CANCEL/BYE retransmits the crossed deadline
//! emits (the stuck c-realign re-INVITE is still retransmitting toward C when
//! begin-termination fires, and may emit a CANCEL during teardown). The
//! assertion that all three legs are BYE'd is NOT relaxed.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5667;

fn x_api_allow_c() -> String {
    format!(r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#)
}

fn refer_to_charlie() -> String {
    format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>")
}

fn assert_notify(txn: &ServerTxn, prefix: &str, frag: &str) {
    let req = txn.request();
    assert_eq!(req.method, "NOTIFY", "expected NOTIFY");
    assert_eq!(get_header(&req.headers, "event").unwrap_or(""), "refer", "NOTIFY Event: refer");
    let ss = get_header(&req.headers, "subscription-state").unwrap_or("");
    assert!(ss.starts_with(prefix), "subscription-state {ss:?} should start with {prefix:?}");
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains(frag), "sipfrag body {body:?} should contain {frag:?}");
}

// ── Overall-safety timer fires while stuck in c-realigning → rollback ─────

#[tokio::test(start_paused = true)]
async fn refer_overall_safety_fires() {
    let h = Harness::new("refer-overall-safety-fires");
    let alice = h.agent("alice", "127.0.0.1:5751").await;
    let bob = h.agent("bob", "127.0.0.1:5761").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    // reinvite_answer=600s, overall_safety=10s → the overall watchdog trips
    // first while the c-realign re-INVITE toward C is unanswered.
    let b2bua = B2buaSut::route_all_with_refer_timers(
        &h, "b2bua", "127.0.0.1:5771", "127.0.0.1", 5761, 600, 10,
    )
    .await;

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
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // C answers its initial INVITE 200 → phase c-realigning; B2BUA ACKs.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;
    let mut charlie_dialog = charlie_uas.dialog();

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 200");
    nterm.respond(200, "OK").await;

    // Observe the c-realign re-INVITE toward C but do NOT answer it — the
    // transfer is stuck in c-realigning until a timer fires.
    charlie.receive("INVITE").await;

    // Advance just past the 10s overall-safety timer (reinvite_answer is 600s,
    // so the overall watchdog trips first). Keep the window tight so the
    // pending re-INVITE's Timer B (~32s) does not also fire.
    h.advance(Duration::from_secs(10) + Duration::from_millis(500)).await;

    // begin-termination BYEs all three legs. Tolerate the stuck c-realign
    // re-INVITE retransmits / a teardown CANCEL racing the BYEs.
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
    let _ = &mut charlie_dialog;
    let _ = h.finish().await;
}
