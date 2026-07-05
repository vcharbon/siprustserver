//! Transparent in-dialog REFER relay + the compose-time opt-out of the upstream
//! `refer_transfer` seed (newkahneed-019).
//!
//! Two halves of one story:
//!   1. `refer_intercept_wins_under_default_composition` — with the seed PRESENT
//!      (default composition) a bridged B-leg REFER is still INTERCEPTED by
//!      `transfer-intercept-refer` (202 + NOTIFY from the B2BUA), NOT relayed —
//!      the new CORE `relay-refer` is out-ranked by registration order.
//!   2. `refer_relays_transparently_when_core_refer_transfer_excluded` — a SUT
//!      built the way a downstream that owns REFER would (via the spawn/compose
//!      seam, `.without_core_refer_transfer()`) relays the SAME B-leg REFER
//!      transparently to the peer leg: the REFER reaches Alice, her 202 relays
//!      back to Bob, and the implicit-subscription NOTIFY rides the dialog
//!      through in the other direction (RFC 3515).

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const REFER_TO_CHARLIE: &str = "<sip:charlie@example.com>";
const REFERRED_BY: &str = "<sip:bob@example.com>";

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

// ── 1. Seed PRESENT → the bridged B-leg REFER is intercepted (precedence). ────

#[tokio::test(start_paused = true)]
async fn refer_intercept_wins_under_default_composition() {
    let h = Harness::new("refer-intercept-wins-default");
    let alice = h.agent("alice", "127.0.0.1:5786").await;
    let bob = h.agent("bob", "127.0.0.1:5787").await;
    // Default composition — the `refer_transfer` seed is present.
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5787)
        .start(&h, "b2bua", "127.0.0.1:5788")
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

    // Bridged B-leg REFER. Even though CORE `relay-refer` now exists, the seed's
    // `transfer-intercept-refer` (also CORE, registered earlier) out-ranks it —
    // so the B2BUA intercepts: it answers 202 ITSELF and drives the transfer
    // machine (NOTIFY 100 active, then NOTIFY 403 terminated from the scripted
    // /call/refer reject). Alice never sees the REFER.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("X-Api-Call", &x_api_call("refer-reject-403"))
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active", "SIP/2.0 100 Trying");
    n100.respond(200, "OK").await;

    let mut nterm = bob.receive("NOTIFY").await;
    assert_notify(&nterm, "terminated", "SIP/2.0 403 Forbidden");
    nterm.respond(200, "OK").await;

    // A↔B undisturbed.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}

// ── 2. Seed EXCLUDED → the SAME bridged B-leg REFER relays transparently. ─────

#[tokio::test(start_paused = true)]
async fn refer_relays_transparently_when_core_refer_transfer_excluded() {
    let h = Harness::new("refer-transparent-relay");
    let alice = h.agent("alice", "127.0.0.1:5792").await;
    let bob = h.agent("bob", "127.0.0.1:5793").await;
    // Opt out of the upstream `refer_transfer` seed the way a downstream that
    // owns REFER via its own transfer machine would — through the compose/spawn
    // seam, not by poking the rule table.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5793)
        .without_core_refer_transfer()
        .start(&h, "b2bua", "127.0.0.1:5794")
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

    // Bob REFERs. With the seed excluded there is no `transfer-intercept-refer`,
    // so `relay-refer` forwards it transparently to the peer leg (Alice). The
    // Refer-To / Referred-By ride through verbatim (relay passthrough).
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("Referred-By", REFERRED_BY)
        .send()
        .await;

    let mut alice_refer = alice.receive("REFER").await;
    let relayed = alice_refer.request();
    assert_eq!(
        get_header(&relayed.headers, "refer-to"),
        Some(REFER_TO_CHARLIE),
        "relayed REFER must carry Refer-To verbatim"
    );
    assert_eq!(
        get_header(&relayed.headers, "referred-by"),
        Some(REFERRED_BY),
        "relayed REFER must carry Referred-By verbatim"
    );
    // Alice (the recipient) accepts; the 202 relays back to Bob's REFER txn.
    alice_refer.respond(202, "Accepted").await;
    refer.expect(202).await;

    // The RFC 3515 implicit subscription rides the dialog: Alice NOTIFYs progress
    // back toward Bob — relayed transparently through `relay-notify`, its 200
    // relayed back through `relay-non-invite-200`.
    let mut notify = alice_dialog
        .send_request(InDialogMethod::Notify)
        .with_header("Event", "refer")
        .with_header("Subscription-State", "active;expires=60")
        .with_header("Content-Type", "message/sipfrag;version=2.0")
        .with_sdp("SIP/2.0 200 OK\r\n")
        .send()
        .await;

    let mut bob_notify = bob.receive("NOTIFY").await;
    assert_notify(&bob_notify, "active", "SIP/2.0 200 OK");
    bob_notify.respond(200, "OK").await;
    notify.expect(200).await;

    // A↔B intact throughout — tear down via Alice BYE.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
}
