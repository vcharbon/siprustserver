//! **A 481 answering a refer NOTIFY ends the subscription it answers** — and
//! nothing else. RFC 6665 §4.4.1 has a `481 Call/Transaction Does Not Exist`
//! answering a NOTIFY terminate the SUBSCRIPTION, not the dialog it rides in;
//! RFC 3515 §2.4.6's implicit REFER subscription rides in the INVITE dialog the
//! call is made of, so the call and the transfer both stand: A↔B is not torn
//! down, and C's answer still drives the c-realign/a-realign merge to
//! "transfer-completed". The one thing the 481 does end is the referrer's
//! implicit subscription: no further NOTIFY leaves on it — not the terminal
//! `Subscription-State: terminated` one either.
//!
//! Scenario: A↔B established, B REFERs to C, the B2BUA's first progress NOTIFY
//! (`active` / `SIP/2.0 100 Trying`) is answered `481` by the referrer. C then
//! answers its INVITE 200 and the transfer completes normally; the referrer's
//! socket must hold no second NOTIFY.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{Event, SubscriptionState};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const CHARLIE_PORT: u16 = 6073;

fn x_api_allow_c() -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    )
}

fn refer_to_charlie() -> String {
    format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>")
}

fn assert_notify(txn: &ServerTxn, state: &str) {
    let req = txn.request();
    assert_eq!(req.method(), "NOTIFY", "expected NOTIFY");
    let event = req.header::<Event>().expect("NOTIFY carries an Event").expect("readable Event");
    assert!(event.is("refer"), "NOTIFY Event: refer, got {:?}", event.token());
    let ss = req
        .header::<SubscriptionState>()
        .expect("NOTIFY carries a Subscription-State")
        .expect("readable Subscription-State");
    assert!(ss.is(state), "subscription-state {:?} should be {state:?}", ss.token());
}

/// Renders the stray NOTIFY's identity for the failure message.
fn describe_notify(txn: &ServerTxn) -> String {
    let req = txn.request();
    let event = req.header::<Event>().and_then(|e| e.ok()).map(|e| e.token().to_string());
    let state =
        req.header::<SubscriptionState>().and_then(|s| s.ok()).map(|s| s.token().to_string());
    format!("{} Event={event:?} Subscription-State={state:?}", req.method())
}

#[tokio::test]
async fn notify_481_ends_the_subscription() {
    let h = Harness::with_transit_delay("refer-notify-481", 1);
    let alice = h.agent("alice", "127.0.0.1:6071").await;
    let bob = h.agent("bob", "127.0.0.1:6072").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6072)
        .start(&h, "b2bua", "127.0.0.1:6074")
        .await;

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

    // ── B REFERs to C → 202 ──────────────────────────────────────────────────
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &refer_to_charlie())
        .with_header("X-Api-Call", &x_api_allow_c())
        .send()
        .await;
    refer.expect(202).await;

    // ── the named deviation: the referrer answers the first NOTIFY 481 ───────
    let mut n100 = bob.receive("NOTIFY").await;
    assert_notify(&n100, "active");
    n100.respond(481, "Call/Transaction Does Not Exist").await;

    // ── the transfer runs its course, untouched by the 481 ───────────────────
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    charlie.receive("ACK").await;

    // c-realign, then a-realign → merge(a, c). Reaching them proves the B2BUA
    // fully processed C's 200 — the batch that emits the terminal NOTIFY today.
    let mut c_realign = charlie.receive("INVITE").await;
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    // Any NOTIFY emitted alongside the realign has landed at 1 ms transit.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── the call stands (a 481 denies the subscription, not the dialog) ──────
    assert!(
        alice.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a NOTIFY's 481 ends the subscription, not the dialog it rides in (RFC 6665 §4.4.1): \
         the caller's established call stands",
    );

    // ── the subscription is over: no further NOTIFY on it ────────────────────
    let stray = bob.try_receive_tolerating("NOTIFY", &["OPTIONS"]).await;
    assert!(
        stray.is_none(),
        "a 481 to a refer NOTIFY ends the subscription (RFC 6665 §4.4.1): no further NOTIFY \
         leaves on it, got {}",
        stray.as_ref().map(describe_notify).unwrap_or_default(),
    );

    // The referrer's dialog stands too.
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the referrer's dialog stands too — nothing in the 481 denied it",
    );

    // ── teardown: A hangs up; the merge sends BYE to C and the orphan B ──────
    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.cdr_records().iter().any(|c| !c.events.is_empty())).await;
    b2bua.assert_fully_reaped();

    // The transfer's own outcome is unchanged by the 481.
    let cdrs = b2bua.cdr_records();
    assert!(
        cdrs.iter()
            .any(|c| c.events.iter().any(|e| e.reason.as_deref() == Some("transfer-completed"))),
        "the transfer completes despite the referrer's 481: {cdrs:?}",
    );
}
