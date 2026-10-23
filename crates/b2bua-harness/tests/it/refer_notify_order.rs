//! The order of this stack's requests toward a REFER referrer. A request with
//! a lower CSeq that arrives after a higher one is refused (RFC 3261 §12.2.2),
//! so a request after one of this stack's NOTIFYs on the referrer's dialog
//! waits for that NOTIFY's final, at most 8·T1 after it left:
//!
//! 1. the final NOTIFY waits for the answer to the immediate one;
//! 2. it leaves 8·T1 after the immediate one when that draws no answer;
//! 3. the BYE the call's end sends the referrer waits for the answer to the
//!    final NOTIFY;
//! 4. it leaves 8·T1 after the final NOTIFY when that draws no answer;
//! 5. it is never sent once the referrer's own BYE ended the dialog;
//! 6. a held NOTIFY still leaves once the referrer's own BYE ended the dialog,
//!    and the call ends only after it;
//! 7. a request relayed to the referrer meanwhile never overtakes the held
//!    NOTIFY's number: a held request is numbered when it leaves;
//! 8. a held NOTIFY never leaves once the answer to the one ahead of it ended
//!    their subscription (RFC 6665 §4.2.2), and the BYE ending the call does.
//!
//! Each referrer request arrives numbered right after the one before it.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::agent::{Agent, Dialog, ServerTxn};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::SubscriptionState;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// The transfer target's answer to its INVITE: its own session, which its
/// later answers on that dialog continue (RFC 3264 §8).
const CHARLIE_FIRST_ANSWER: &str = "v=0\r\no=charlie 9 8 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CHARLIE_ACTIVE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// 8·T1: how long a request waits behind an unanswered NOTIFY.
const ANSWER_CAP: Duration = Duration::from_millis(4_000);

fn x_api_allow(charlie_port: u16) -> String {
    format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{charlie_port}}}}}"#
    )
}

fn x_api_reject() -> String {
    r#"{"refer_key":"refer-reject-403"}"#.to_string()
}

fn cseq(txn: &ServerTxn) -> u32 {
    txn.request().cseq().seq()
}

fn assert_subscription(txn: &ServerTxn, state: &str) {
    let req = txn.request();
    assert_eq!(req.method(), "NOTIFY", "expected NOTIFY");
    let ss = req
        .header::<SubscriptionState>()
        .expect("NOTIFY carries a Subscription-State")
        .expect("readable Subscription-State");
    assert!(ss.is(state), "subscription-state {:?} should be {state:?}", ss.token());
}

/// Alice ↔ Bob established through the B2BUA; Bob REFERs with `x_api` and the
/// REFER is accepted. Returns Alice's dialog and Bob's.
async fn bob_refers(alice: &Agent, bob: &Agent, b2bua: &B2buaSut, x_api: &str) -> (Dialog, Dialog) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", "<sip:charlie@127.0.0.1>")
        .with_header("X-Api-Call", x_api)
        .send()
        .await;
    refer.expect(202).await;
    (alice_dialog, bob_dialog)
}

/// Charlie answers the transfer and the realign runs to the merge; then Alice
/// hangs up the transferred call and the B2BUA ends Charlie's and Bob's legs.
async fn transfer_completes_and_alice_hangs_up(
    alice: &Agent,
    bob: &Agent,
    charlie: &Agent,
    mut alice_dialog: Dialog,
    mut c_realign: ServerTxn,
) {
    c_realign.respond(200, "OK").with_sdp(CHARLIE_ACTIVE_ANSWER).await;
    charlie.receive("ACK").await;
    let mut a_realign = alice.receive("INVITE").await;
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    alice.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
}

#[tokio::test(start_paused = true)]
async fn the_final_notify_waits_for_the_answer_to_the_immediate_one() {
    let h = Harness::with_transit_delay("refer-final-notify-waits", 1);
    let alice = h.agent("alice", "127.0.0.1:6201").await;
    let bob = h.agent("bob", "127.0.0.1:6202").await;
    let charlie = h.agent("charlie", "127.0.0.1:6203").await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6202)
        .start(&h, "b2bua", "127.0.0.1:6204")
        .await;
    let (alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_allow(6203)).await;

    let mut immediate = bob.receive("NOTIFY").await;
    assert_subscription(&immediate, "active");
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_FIRST_ANSWER).await;
    charlie.receive("ACK").await;
    // The transfer goes on meanwhile: C is realigned.
    let c_realign = charlie.receive("INVITE").await;

    h.advance(Duration::from_secs(1)).await;
    assert!(
        bob.try_receive_tolerating("NOTIFY", &[]).await.is_none(),
        "the final NOTIFY waits while the immediate one is unanswered"
    );

    immediate.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    assert_subscription(&last, "terminated");
    assert_eq!(cseq(&last), cseq(&immediate) + 1);
    last.respond(200, "OK").await;

    transfer_completes_and_alice_hangs_up(&alice, &bob, &charlie, alice_dialog, c_realign).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_final_notify_leaves_at_the_cap_when_the_immediate_one_is_unanswered() {
    let h = Harness::with_transit_delay("refer-final-notify-cap", 1);
    let alice = h.agent("alice", "127.0.0.1:6211").await;
    let bob = h.agent("bob", "127.0.0.1:6212").await;
    let charlie = h.agent("charlie", "127.0.0.1:6213").await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6212)
        .start(&h, "b2bua", "127.0.0.1:6214")
        .await;
    let (alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_allow(6213)).await;

    let mut immediate = bob.receive("NOTIFY").await;
    let sent = Instant::now();
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_FIRST_ANSWER).await;
    charlie.receive("ACK").await;
    let c_realign = charlie.receive("INVITE").await;

    let mut last = bob.receive("NOTIFY").await;
    assert_eq!(Instant::now() - sent, ANSWER_CAP, "the final NOTIFY leaves at the cap");
    assert_subscription(&last, "terminated");
    assert_eq!(cseq(&last), cseq(&immediate) + 1);
    immediate.respond(200, "OK").await;
    last.respond(200, "OK").await;

    transfer_completes_and_alice_hangs_up(&alice, &bob, &charlie, alice_dialog, c_realign).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_bye_ending_the_call_waits_for_the_answer_to_the_final_notify() {
    let h = Harness::with_transit_delay("refer-bye-waits-for-final-notify", 1);
    let alice = h.agent("alice", "127.0.0.1:6221").await;
    let bob = h.agent("bob", "127.0.0.1:6222").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6222)
        .start(&h, "b2bua", "127.0.0.1:6224")
        .await;
    let (mut alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    assert_subscription(&last, "terminated");

    // Alice hangs up while the final NOTIFY is unanswered.
    alice_dialog.bye().await.expect(200).await;
    h.advance(Duration::from_secs(1)).await;
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the BYE waits while the final NOTIFY is unanswered"
    );

    last.respond(200, "OK").await;
    let mut bye = bob.receive("BYE").await;
    assert_eq!(cseq(&bye), cseq(&last) + 1);
    bye.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_bye_ending_the_call_leaves_at_the_cap_when_the_final_notify_is_unanswered() {
    let h = Harness::with_transit_delay("refer-bye-cap", 1);
    let alice = h.agent("alice", "127.0.0.1:6231").await;
    let bob = h.agent("bob", "127.0.0.1:6232").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6232)
        .start(&h, "b2bua", "127.0.0.1:6234")
        .await;
    let (mut alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    let sent = Instant::now();

    alice_dialog.bye().await.expect(200).await;
    let mut bye = bob.receive("BYE").await;
    assert_eq!(Instant::now() - sent, ANSWER_CAP, "the BYE leaves at the cap");
    assert_eq!(cseq(&bye), cseq(&last) + 1);
    last.respond(200, "OK").await;
    bye.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_bye_held_is_never_sent_once_the_referrer_ended_the_dialog() {
    let h = Harness::with_transit_delay("refer-held-bye-dropped", 1);
    let alice = h.agent("alice", "127.0.0.1:6241").await;
    let bob = h.agent("bob", "127.0.0.1:6242").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6242)
        .start(&h, "b2bua", "127.0.0.1:6244")
        .await;
    let (mut alice_dialog, mut bob_dialog) =
        bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    alice_dialog.bye().await.expect(200).await;

    // Bob hangs up himself while our BYE waits on his answer to the NOTIFY.
    bob_dialog.bye().await.expect(200).await;
    last.respond(200, "OK").await;
    h.advance(ANSWER_CAP).await;
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "no BYE follows the referrer's own"
    );

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_held_notify_still_leaves_once_the_referrer_ended_the_dialog() {
    let h = Harness::with_transit_delay("refer-held-notify-after-referrer-bye", 1);
    let alice = h.agent("alice", "127.0.0.1:6251").await;
    let bob = h.agent("bob", "127.0.0.1:6252").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6252)
        .start(&h, "b2bua", "127.0.0.1:6254")
        .await;
    let (mut alice_dialog, mut bob_dialog) =
        bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    // The final NOTIFY waits behind the unanswered immediate one, Alice's BYE
    // toward Bob behind it.
    let mut immediate = bob.receive("NOTIFY").await;
    alice_dialog.bye().await.expect(200).await;
    bob_dialog.bye().await.expect(200).await;

    immediate.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    assert_subscription(&last, "terminated");
    assert_eq!(cseq(&last), cseq(&immediate) + 1);
    assert!(!b2bua.is_reaped(), "the call is not over while a request waits to leave");
    last.respond(481, "Call/Transaction Does Not Exist").await;
    h.advance(ANSWER_CAP).await;
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "no BYE follows the referrer's own"
    );

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_relayed_meanwhile_never_overtakes_the_held_notify_s_number() {
    let h = Harness::with_transit_delay("refer-relayed-request-numbering", 1);
    let alice = h.agent("alice", "127.0.0.1:6261").await;
    let bob = h.agent("bob", "127.0.0.1:6262").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6262)
        .start(&h, "b2bua", "127.0.0.1:6264")
        .await;
    let (mut alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    // The reject's final NOTIFY waits behind the unanswered immediate one.
    let mut immediate = bob.receive("NOTIFY").await;
    h.advance(Duration::from_millis(100)).await;
    let mut a_info = alice_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/dtmf-relay")
        .with_sdp("Signal=5\r\nDuration=160\r\n")
        .send()
        .await;
    let mut bob_info = bob.receive("INFO").await;
    assert_eq!(cseq(&bob_info), cseq(&immediate) + 1, "the INFO takes the next number");
    bob_info.respond(200, "OK").await;
    a_info.expect(200).await;

    immediate.respond(200, "OK").await;
    let mut last = bob.receive("NOTIFY").await;
    assert_eq!(cseq(&last), cseq(&bob_info) + 1, "the held NOTIFY is numbered as it leaves");
    last.respond(200, "OK").await;

    alice_dialog.bye().await.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_held_notify_never_leaves_once_the_answer_ahead_ended_the_subscription() {
    let h = Harness::with_transit_delay("refer-held-notify-after-481", 1);
    let alice = h.agent("alice", "127.0.0.1:6271").await;
    let bob = h.agent("bob", "127.0.0.1:6272").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 6272)
        .start(&h, "b2bua", "127.0.0.1:6274")
        .await;
    let (mut alice_dialog, _) = bob_refers(&alice, &bob, &b2bua, &x_api_reject()).await;

    // The reject's final NOTIFY waits behind the unanswered immediate one.
    let mut immediate = bob.receive("NOTIFY").await;
    h.advance(Duration::from_secs(1)).await;
    immediate.respond(481, "Call/Transaction Does Not Exist").await;
    h.advance(ANSWER_CAP).await;
    assert!(
        bob.try_receive_tolerating("NOTIFY", &[]).await.is_none(),
        "a 481 to a NOTIFY ends its subscription: the NOTIFY held behind it never leaves"
    );

    alice_dialog.bye().await.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
