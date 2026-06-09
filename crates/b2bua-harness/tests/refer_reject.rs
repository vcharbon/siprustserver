//! REFER reject-path scenarios (slice 5e). Port of
//! `tests/scenarios/refer-reject.ts`.
//!
//! Each scenario brings an A↔B call to confirmed state, then exercises one
//! reject path of the TransferRules chain:
//!   1. referRejectHttp403  — HTTP reject 403 → NOTIFY 100 active + NOTIFY 403 terminated.
//!   2. referHttpTimeout    — HTTP hangs → 60s subscription-expiry → NOTIFY 500 terminated.
//!   3. referReplacesRejected — Refer-To carries Replaces= → REFER 501 (seed rule).
//!   4. referOutOfDialog    — REFER on an unknown Call-ID → 481 (router pre-rule path).
//!   5. referSecondDuringAuthorizing — second REFER while refer-authorizing → 491.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const REFER_TO_CHARLIE: &str = "<sip:charlie@example.com>";

/// The `X-Api-Call` instruction JSON for a given `refer_key`.
fn x_api_call(key: &str) -> String {
    format!(r#"{{"refer_key":"{key}"}}"#)
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

// ── 1. REFER → X-Api-Call refer-reject-403 → NOTIFY 100 active, NOTIFY 403 term ─

#[tokio::test]
async fn refer_reject_http_403() {
    let h = Harness::with_transit_delay("refer-reject-http-403", 1);
    let alice = h.agent("alice", "127.0.0.1:5701").await;
    let bob = h.agent("bob", "127.0.0.1:5711").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5721", "127.0.0.1", 5711).await;

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

    // REFER; the scripted /call/refer replies reject 403.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("X-Api-Call", &x_api_call("refer-reject-403"))
        .send()
        .await;
    refer.expect(202).await;

    // NOTIFY 100 active (implicit subscription).
    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // NOTIFY terminated — sipfrag status matches the HTTP reject code (403).
    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 403 Forbidden");
    nterm.respond(200, "OK").await;

    // A↔B undisturbed — tear it down normally.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 2. REFER → X-Api-Call refer-http-timeout → 60s later NOTIFY 500 terminated ─

#[tokio::test(start_paused = true)]
async fn refer_http_timeout() {
    let h = Harness::new("refer-http-timeout");
    let alice = h.agent("alice", "127.0.0.1:5702").await;
    let bob = h.agent("bob", "127.0.0.1:5712").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5722", "127.0.0.1", 5712).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // The scripted /call/refer hangs forever on this key → the transfer stays in
    // refer-authorizing until the 60s subscription-expiry timer fires.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("X-Api-Call", &x_api_call("refer-http-timeout"))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // Advance toward the 60s subscription-expiry. The 30s keepalive cycle fires
    // first; answer the OPTIONS on both legs to keep the call up.
    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // Cross the 60s sub-expiry → NOTIFY 500 terminated on B. The 60s keepalive
    // OPTIONS fires in the same advance; receive B's NOTIFY tolerating OPTIONS.
    h.advance(Duration::from_secs(30)).await;
    let mut nterm = bob.receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert_notify(&nterm, "terminated", "SIP/2.0 500");
    nterm.respond(200, "OK").await;

    // A↔B intact (the transfer resolved at sub-expiry); tear down via A BYE.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY", "OPTIONS"]).await.respond(200, "OK").await;
    alice_bye.expect_tolerating(200, &["OPTIONS"]).await;

    let _ = h.finish().await;
}

// ── 3. REFER with Replaces= in Refer-To → 501 Not Implemented ─────────────

#[tokio::test]
async fn refer_replaces_rejected() {
    let h = Harness::with_transit_delay("refer-replaces-rejected", 1);
    let alice = h.agent("alice", "127.0.0.1:5703").await;
    let bob = h.agent("bob", "127.0.0.1:5713").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5723", "127.0.0.1", 5713).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // Attended-transfer REFER (RFC 3891). Not supported — the seed rule
    // transfer-reject-replaces responds 501 directly (no subscription/NOTIFY).
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header(
            "Refer-To",
            "<sip:charlie@example.com?Replaces=abc%3Bto-tag%3Dx%3Bfrom-tag%3Dy>",
        )
        .send()
        .await;
    refer.expect(501).await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 4. Out-of-dialog REFER (unknown Call-ID) → 481 ────────────────────────

#[tokio::test]
async fn refer_out_of_dialog() {
    let h = Harness::with_transit_delay("refer-out-of-dialog", 1);
    // This scenario DELIBERATELY sends an out-of-dialog REFER carrying a bogus
    // To-tag (a §8.1.1.2 / MUST-016 violation) to drive the router's orphan-reject
    // 481 path — exactly the kind of intentional non-compliance the RFC audit's
    // `allow_violation` waiver exists for. The stranger, not the B2BUA, is the
    // non-conformant party here.
    h.allow_violation(
        "rfc3261.noToTagOnInitialRequest",
        "stranger deliberately sends an out-of-dialog REFER with a bogus To-tag to \
         exercise the router's unknown-dialog 481 reject",
    );
    let alice = h.agent("alice", "127.0.0.1:5704").await;
    let bob = h.agent("bob", "127.0.0.1:5714").await;
    let stranger = h.agent("stranger", "127.0.0.1:5734").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5724", "127.0.0.1", 5714).await;

    // Keep a real A↔B call alive so the B2BUA has routing state around.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // Stranger sends a REFER on an unknown dialog (fresh Call-ID, bogus to-tag,
    // no stamped callref on the R-URI) — dialog resolution fails, so the router
    // rejects 481 before any transfer rule runs (maybe_reject_orphan).
    let mut stranger_refer = stranger.send_out_of_dialog_refer(b2bua.addr, REFER_TO_CHARLIE).await;
    stranger_refer.expect(481).await;

    // Active call teardown proves unrelated dialogs are unaffected.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    let _ = &mut bob_dialog;

    let _ = h.finish().await;
}

// ── 5. Second REFER during refer-authorizing → 491 ────────────────────────

#[tokio::test(start_paused = true)]
async fn refer_second_during_authorizing() {
    let h = Harness::new("refer-second-during-authorizing");
    let alice = h.agent("alice", "127.0.0.1:5705").await;
    let bob = h.agent("bob", "127.0.0.1:5715").await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5725", "127.0.0.1", 5715).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // First REFER — HTTP hangs so the transfer stays in refer-authorizing.
    let mut first_refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("X-Api-Call", &x_api_call("refer-http-timeout"))
        .send()
        .await;
    first_refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    // Second REFER while still refer-authorizing → 491 (transfer-reject-second-refer).
    let mut second_refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .send()
        .await;
    second_refer.expect(491).await;

    // Advance past the 60s subscription-expiry so the first REFER resolves
    // cleanly (NOTIFY 500 terminated). Answer the 30s keepalive cycle first.
    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    h.advance(Duration::from_secs(30)).await;
    let mut nterm = bob.receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert_notify(&nterm, "terminated", "SIP/2.0 500");
    nterm.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY", "OPTIONS"]).await.respond(200, "OK").await;
    alice_bye.expect_tolerating(200, &["OPTIONS"]).await;

    let _ = h.finish().await;
}
