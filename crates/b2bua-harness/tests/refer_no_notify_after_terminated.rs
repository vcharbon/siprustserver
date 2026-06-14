//! **No NOTIFY after terminal (REFER-NOTIFY-9)** — once the transfer's terminal
//! NOTIFY (`Subscription-State: terminated`) has been sent and the transfer slice
//! is cleared (the `transfer-a-realign-200` merge does `SetTransfer { state: None }`,
//! removing the cursor → every transfer rule deactivates, `refer_transfer.rs`), a
//! LATE event on the (now plain, post-transfer) C leg must NOT produce another
//! NOTIFY toward the referrer. The referrer's implicit subscription is over; the
//! B2BUA is back to being an ordinary bridge for A↔C.
//!
//! Scenario: drive the full happy transfer to completion (merge a↔c, two NOTIFYs
//! seen by bob: `active`/100-Trying then `terminated`/200). Then C — now an
//! ordinary bridged peer — fires an in-dialog re-INVITE. The transfer machine is
//! gone, so this is relayed to A as a vanilla re-INVITE and bob receives ZERO
//! further NOTIFY. We assert bob's socket holds no NOTIFY after the late event is
//! fully processed.
//!
//! (REFER-SCOPE-4 — REFER retransmission idempotency — remains DEFERRED: it needs
//! a harness primitive to resend an identical branch/CSeq REFER, which the DSL
//! lacks.)

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
// C's answer to the LATE post-transfer re-INVITE.
const CHARLIE_LATE_ANSWER: &str = "v=0\r\no=charlie 10 10 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 5668;

fn x_api_allow_c() -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    )
}

fn refer_to_charlie() -> String {
    format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>")
}

fn assert_notify(txn: &ServerTxn, prefix: &str) {
    let req = txn.request();
    assert_eq!(req.method, "NOTIFY", "expected NOTIFY");
    assert_eq!(get_header(&req.headers, "event").unwrap_or(""), "refer", "NOTIFY Event: refer");
    let ss = get_header(&req.headers, "subscription-state").unwrap_or("");
    assert!(ss.starts_with(prefix), "subscription-state {ss:?} should start with {prefix:?}");
}

#[tokio::test]
async fn no_notify_after_terminated() {
    let h = Harness::with_transit_delay("refer-no-notify-after-terminated", 1);
    let alice = h.agent("alice", "127.0.0.1:5966").await;
    let bob = h.agent("bob", "127.0.0.1:5976").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer(&h, "b2bua", "127.0.0.1:5986", "127.0.0.1", 5976).await;

    // ── A↔B established ──────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    // ── REFER → 202, then the active + terminal NOTIFYs ──────────────────────
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active");
    n100.respond(200, "OK").await;

    // C answers initial INVITE → B2BUA ACKs; terminal NOTIFY fires.
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated"); // ← the TERMINAL NOTIFY
    nterm.respond(200, "OK").await;

    // c-realign + a-realign → merge(a, c). The merge clears the transfer slice.
    let mut c_realign = charlie.receive("INVITE").await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    // Drain anything still queued at bob from the (legitimate) transfer phase so
    // the post-terminal assertion only sees LATE traffic.
    settle_until(|| b2bua.cdr_records().iter().any(|c| !c.events.is_empty())).await;
    bob.drain().await;

    // ── LATE C event: C (now an ordinary bridged peer) fires a re-INVITE ─────
    // The transfer machine is gone (cursor cleared at merge), so this is a plain
    // in-dialog re-INVITE relayed to A — NOT a transfer event. Build C's dialog
    // from its initial-INVITE UAS transaction.
    let mut charlie_dialog = charlie_uas.dialog();
    let mut c_reinvite = charlie_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(CHARLIE_LATE_ANSWER)
        .send()
        .await;

    // The B2BUA relays the re-INVITE to A (its new bridged peer), not to bob.
    let mut a_reinvite = alice.receive("INVITE").await;
    a_reinvite.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    c_reinvite.expect(200).await;
    charlie_dialog.ack(None).await;
    alice.receive("ACK").await;

    // ── ASSERT: bob (the referrer) received ZERO further NOTIFY ──────────────
    // The late re-INVITE round-trip above (relayed to A, 200 back to C, ACK) proves
    // the B2BUA fully processed the late C event. Had a transfer rule still been
    // live it would have emitted a NOTIFY synchronously on that event; poll bob's
    // socket and require it empty of NOTIFY.
    let stray = bob.try_receive_tolerating("NOTIFY", &["OPTIONS", "INVITE", "BYE"]).await;
    assert!(stray.is_none(), "no NOTIFY toward the referrer after the terminal NOTIFY + slice clear");

    let _ = &mut alice_dialog;
    let _ = h.finish().await;
}
