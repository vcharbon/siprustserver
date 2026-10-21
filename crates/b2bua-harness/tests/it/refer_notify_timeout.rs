//! **A refer NOTIFY that draws no final ends its subscription, not the call.**
//! RFC 6665 §4.2.2 has a notifier whose NOTIFY fails on Timer F remove the
//! subscription; RFC 5057 §5.2 confines a transaction timeout to the usage it
//! occurred in. The implicit REFER subscription (RFC 3515 §2.4.6) shares the
//! INVITE dialog of the call, so the INVITE usage stands: A↔B stay up.
//!
//! Scenario: A↔B established, B REFERs, the decision rejects the transfer.
//! The referrer answers the `active` NOTIFY and never answers the terminating
//! one (the silent referrer is the test target). Past its Timer F (64·T1 =
//! 32 s) the call is still up and A hangs up normally.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::agent::ServerTxn;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::SubscriptionState;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

fn assert_state(txn: &ServerTxn, state: &str) {
    let ss = txn
        .request()
        .header::<SubscriptionState>()
        .expect("NOTIFY carries a Subscription-State")
        .expect("readable Subscription-State");
    assert!(ss.is(state), "subscription-state {:?} should be {state:?}", ss.token());
}

#[tokio::test(start_paused = true)]
async fn unanswered_notify_leaves_the_call_up() {
    let h = Harness::new("refer-notify-timeout");
    let alice = h.agent("alice", "127.0.0.1:6081").await;
    let bob = h.agent("bob", "127.0.0.1:6082").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6082)
        .start(&h, "b2bua", "127.0.0.1:6084")
        .await;

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
        .with_header("Refer-To", "<sip:charlie@example.com>")
        .with_header("X-Api-Call", r#"{"refer_key":"refer-reject-403"}"#)
        .send()
        .await;
    refer.expect(202).await;

    let mut n100 = bob.receive("NOTIFY").await;
    assert_state(&n100, "active");
    n100.respond(200, "OK").await;

    // The terminating NOTIFY is never answered.
    let nterm = bob.receive("NOTIFY").await;
    assert_state(&nterm, "terminated");

    // The keepalive at 30 s finds both parties alive.
    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // Past the NOTIFY's Timer F (32 s after it left).
    h.advance(Duration::from_secs(3)).await;
    assert!(
        alice.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a NOTIFY's timeout ends its subscription, not the call (RFC 5057 §5.2)",
    );
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the referrer's INVITE usage stands (RFC 5057 §5.2)",
    );
    assert_eq!(b2bua.active_calls(), 1, "the call survives the dead NOTIFY");
    drop(nterm);

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
