//! Transparent in-dialog REFER relay — the DEFAULT — plus the two ways a
//! deployment leaves it: the per-call `features.refer` directive and the
//! compose-time opt-out of the upstream `refer_transfer` seed.
//!
//! Four faces of one story:
//!   1. `refer_intercept_wins_when_the_route_activates_local_processing` — a
//!      route carrying the `features.refer` arm makes a bridged B-leg REFER
//!      INTERCEPTED by `transfer-intercept-refer` (202 + NOTIFY from the
//!      B2BUA), never relayed: the seed out-ranks `relay-refer` by registration
//!      order.
//!   2. `refer_relays_transparently_when_the_route_activates_nothing` — the
//!      SAME default composition, a route that activates no REFER feature: the
//!      REFER reaches Alice, her 202 relays back to Bob, and the
//!      implicit-subscription NOTIFY rides the dialog through the other way
//!      (RFC 3515).
//!   3. `a_malformed_refer_to_relays_untouched_on_the_transparent_path` — the
//!      relay reads no Refer-To syntax: a header no reader accepts crosses
//!      verbatim and the far end's own 400 comes back to the transferor.
//!   4. `refer_relays_transparently_when_core_refer_transfer_excluded` — a SUT
//!      built the way a downstream that owns REFER would (via the spawn/compose
//!      seam, `.without_core_refer_transfer()`) relays the REFER even on a call
//!      whose route DID activate the feature.

use b2bua_harness::B2buaSut;
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{Event, HeaderName, SubscriptionState};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const REFER_TO_CHARLIE: &str = "<sip:charlie@example.com>";
const REFERRED_BY: &str = "<sip:bob@example.com>";

/// The `X-Api-Call` instruction JSON for a given `refer_key`.
fn x_api_call(key: &str) -> String {
    format!(r#"{{"refer_key":"{key}"}}"#)
}

/// Assert a received NOTIFY carries `Event: refer`, the `Subscription-State`
/// `state`, and a sipfrag body containing `frag`.
fn assert_notify(txn: &ServerTxn, state: &str, frag: &str) {
    let req = txn.request();
    assert_eq!(req.method(), "NOTIFY", "expected NOTIFY");
    let event = req.header::<Event>().expect("NOTIFY carries an Event").expect("readable Event");
    assert!(event.is("refer"), "NOTIFY Event: refer, got {:?}", event.token());
    let ss = req
        .header::<SubscriptionState>()
        .expect("NOTIFY carries a Subscription-State")
        .expect("readable Subscription-State");
    assert!(ss.is(state), "subscription-state {:?} should be {state:?}", ss.token());
    let body = String::from_utf8_lossy(req.body());
    assert!(body.contains(frag), "sipfrag body {body:?} should contain {frag:?}");
}

// ── 1. Route activates `features.refer` → the REFER is intercepted. ──────────

#[tokio::test(start_paused = true)]
async fn refer_intercept_wins_when_the_route_activates_local_processing() {
    let h = Harness::new("refer-intercept-wins-local");
    let alice = h.agent("alice", "127.0.0.1:5786").await;
    let bob = h.agent("bob", "127.0.0.1:5787").await;
    // Default composition, and a route whose `features.refer` arm directs local
    // REFER processing.
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

    // Bridged B-leg REFER on a call directed to be processed locally: the seed's
    // `transfer-intercept-refer` (CORE, registered before `relay-refer`) wins —
    // the B2BUA answers 202 ITSELF and drives the transfer machine (NOTIFY 100
    // active, then NOTIFY 403 terminated from the scripted /call/refer reject).
    // Alice never sees the REFER.
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

// ── 2. Route activates nothing → the SAME REFER relays to the peer leg. ──────

#[tokio::test(start_paused = true)]
async fn refer_relays_transparently_when_the_route_activates_nothing() {
    let h = Harness::new("refer-transparent-no-directive");
    let alice = h.agent("alice", "127.0.0.1:5795").await;
    let bob = h.agent("bob", "127.0.0.1:5796").await;
    // Default composition — the `refer_transfer` seed IS present. What is absent
    // is the route's `features.refer` arm, so this platform processes no
    // transfer and the REFER is an ordinary in-dialog request.
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5796).start(&h, "b2bua", "127.0.0.1:5797").await;

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

    // Bob REFERs. `relay-refer` forwards it to the peer leg with Refer-To /
    // Referred-By verbatim — the same treatment INFO gets.
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("Referred-By", REFERRED_BY)
        .send()
        .await;

    let mut alice_refer = alice.receive("REFER").await;
    let relayed = alice_refer.request();
    assert_eq!(relayed.raw(HeaderName::ReferTo).next(), Some(REFER_TO_CHARLIE));
    assert_eq!(relayed.raw(HeaderName::ReferredBy).next(), Some(REFERRED_BY));
    alice_refer.respond(202, "Accepted").await;
    refer.expect(202).await;

    // The RFC 3515 implicit subscription rides the dialog back the other way.
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
    b2bua.assert_fully_reaped();
}

// ── 3. Transparent path reads no Refer-To syntax. ────────────────────────────

/// A relayed REFER is not a transfer this platform runs, so its Refer-To is not
/// this platform's to read: an unclosed name-addr no reader accepts (RFC 3261
/// §20.30 / §25.1) crosses to the peer verbatim and the answer is the far end's
/// — here Alice's own 400, relayed back to the transferor.
///
/// The malformed header is Bob's and IS the subject of the test; the SUT's own
/// output stays compliant.
#[tokio::test(start_paused = true)]
async fn a_malformed_refer_to_relays_untouched_on_the_transparent_path() {
    const MALFORMED_REFER_TO: &str = "<sip:charlie@example.com";

    let h = Harness::new("refer-transparent-malformed-referto");
    let alice = h.agent("alice", "127.0.0.1:5798").await;
    let bob = h.agent("bob", "127.0.0.1:5799").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5799).start(&h, "b2bua", "127.0.0.1:5800").await;

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
        .with_header("Refer-To", MALFORMED_REFER_TO)
        .send()
        .await;

    // Byte-for-byte what Bob sent, unclosed bracket included.
    let mut alice_refer = alice.receive("REFER").await;
    assert_eq!(
        alice_refer.request().raw(HeaderName::ReferTo).next(),
        Some(MALFORMED_REFER_TO),
        "the relay rewrites no Refer-To"
    );
    // Alice refuses a target she cannot read; her final relays back to Bob.
    alice_refer.respond(400, "Bad Request").await;
    refer.expect(400).await;

    // A↔B intact — the exchange was end-to-end and changed nothing here.
    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _ = h.finish().await;
    b2bua.assert_fully_reaped();
}

// ── 4. Seed EXCLUDED → the REFER relays even with the feature active. ────────

#[tokio::test(start_paused = true)]
async fn refer_relays_transparently_when_core_refer_transfer_excluded() {
    let h = Harness::new("refer-transparent-relay");
    let alice = h.agent("alice", "127.0.0.1:5792").await;
    let bob = h.agent("bob", "127.0.0.1:5793").await;
    // Opt out of the upstream `refer_transfer` seed the way a downstream that
    // owns REFER via its own transfer machine would — through the compose/spawn
    // seam, not by poking the rule table. The route still activates
    // `features.refer`: the compose-time exclusion is the stronger word, so the
    // REFER relays anyway.
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5793)
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

    // Bob REFERs. With the seed excluded there is no `transfer-intercept-refer`
    // at all, so `relay-refer` forwards it to the peer leg (Alice). The
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
        relayed.raw(HeaderName::ReferTo).next(),
        Some(REFER_TO_CHARLIE),
        "relayed REFER must carry Refer-To verbatim"
    );
    assert_eq!(
        relayed.raw(HeaderName::ReferredBy).next(),
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
