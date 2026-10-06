//! The B2BUA's own Contact on the 2xx it sends to a target-refresh or
//! subscription-creating request, answered locally or relayed.
//!
//! - A 2xx to UPDATE is a target refresh (RFC 3311 §5.2): it carries the
//!   answerer's Contact, the remote target the UPDATE's sender adopts.
//! - A 202 to a REFER creates the implicit subscription (RFC 3515 §2.4.2,
//!   RFC 6665 §4.2.1): it carries the notifier's Contact.
//!
//! The Contact a face is shown names that face's leg (`leg=`). A non-2xx final
//! refreshes nothing and carries none (`contact_policy`).

use std::net::SocketAddr;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::{Contact, HeaderValue, ParamValue};
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendonly\r\n";

const REFER_TO_CHARLIE: &str = "<sip:charlie@example.com>";

/// `resp` carries exactly one Contact, the B2BUA's own (`b2bua`) for `leg`.
fn assert_stack_contact(resp: &SipResponse, leg: &str, b2bua: SocketAddr) {
    let contacts = resp.list::<Contact>().expect("readable Contact list");
    assert_eq!(contacts.len(), 1, "one Contact on {} {}", resp.status(), resp.cseq().method());
    let contact = &contacts[0];
    assert_eq!(
        contact.uri().param("leg").and_then(ParamValue::as_str),
        Some(leg),
        "Contact {} names the {leg} face",
        contact.to_wire(),
    );
    assert_eq!(
        (contact.uri().host(), contact.uri().port()),
        (b2bua.ip().to_string().as_str(), Some(b2bua.port())),
        "Contact {} names the B2BUA itself",
        contact.to_wire(),
    );
}

/// `resp` carries no Contact.
fn assert_no_contact(resp: &SipResponse) {
    assert!(
        resp.header::<Contact>().is_none(),
        "no Contact on {} {}",
        resp.status(),
        resp.cseq().method()
    );
}

/// A transfer the B2BUA terminates: Bob's REFER is answered 202 by the
/// B2BUA itself, which then notifies him as the implicit subscription's
/// notifier. The 202 carries the Contact of Bob's face.
#[tokio::test(start_paused = true)]
async fn local_202_to_refer_states_the_stack_contact() {
    let h = Harness::new("local-202-refer-contact");
    let alice = h.agent("alice", "127.0.0.1:7101").await;
    let bob = h.agent("bob", "127.0.0.1:7102").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 7102)
        .start(&h, "b2bua", "127.0.0.1:7103")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .with_header("X-Api-Call", r#"{"refer_key":"refer-reject-403"}"#)
        .send()
        .await;
    let accepted = refer.expect(202).await;
    assert_stack_contact(&accepted, "b-1", b2bua.addr);

    bob.receive("NOTIFY").await.respond(200, "OK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A REFER the B2BUA refuses locally (no readable Refer-To) is answered 400:
/// a non-2xx creates no subscription and carries no Contact.
#[tokio::test(start_paused = true)]
async fn local_400_to_refer_states_no_contact() {
    let h = Harness::new("local-400-refer-no-contact");
    let alice = h.agent("alice", "127.0.0.1:7104").await;
    let bob = h.agent("bob", "127.0.0.1:7105").await;
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 7105)
        .start(&h, "b2bua", "127.0.0.1:7106")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog.send_request(InDialogMethod::Refer).send().await;
    let refused = refer.expect(400).await;
    assert_no_contact(&refused);

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The B2BUA answers Alice's BYE itself: a 2xx to a request that refreshes no
/// target carries no Contact (RFC 3261 §15.1).
#[tokio::test(start_paused = true)]
async fn local_200_to_bye_states_no_contact() {
    let s = B2buaScene::new("local-200-bye-no-contact").await;
    let mut dialog = s.establish().await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    let answered = bye.expect(200).await;
    assert_no_contact(&answered);

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// Under fake PRACK the B2BUA answers Bob's early-dialog UPDATE(offer) itself.
/// The 200 carries the Contact of Bob's face.
#[tokio::test(start_paused = true)]
async fn local_200_to_callee_early_update_states_the_stack_contact() {
    let h = Harness::new("local-200-update-from-callee-contact");
    let alice = h.agent("alice", "127.0.0.1:7107").await;
    let bob = h.agent("bob", "127.0.0.1:7108").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 7108, RelayFirst18xStrategy::FakePrack)
            .start(&h, "b2bua", "127.0.0.1:7109")
            .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    let mut bob_dialog = uas.dialog();
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(ANSWER)).await;
    let answered = update.expect(200).await;
    assert_stack_contact(&answered, "b-1", b2bua.addr);

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Under fake PRACK the B2BUA answers Alice's bodyless early-dialog UPDATE
/// itself, without waking Bob. The 200 carries the Contact of Alice's face.
#[tokio::test(start_paused = true)]
async fn local_200_to_caller_early_update_states_the_stack_contact() {
    let h = Harness::new("local-200-update-from-caller-contact");
    let alice = h.agent("alice", "127.0.0.1:7110").await;
    let bob = h.agent("bob", "127.0.0.1:7111").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 7111, RelayFirst18xStrategy::FakePrack)
            .start(&h, "b2bua", "127.0.0.1:7112")
            .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    let ringing = call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    let a_tag = ringing.to().tag().expect("the 180 names the early dialog").to_string();
    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&a_tag).send().await;
    let answered = update.expect(200).await;
    assert_stack_contact(&answered, "a", b2bua.addr);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A confirmed-dialog UPDATE relays to Bob; his 200 reaches Alice under the
/// Contact of Alice's face, whatever Bob's own 200 stated.
#[tokio::test(start_paused = true)]
async fn relayed_200_to_update_states_the_stack_contact() {
    let s = B2buaScene::new("relayed-200-update-contact").await;
    let mut dialog = s.establish().await;

    let mut update = dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    s.bob.receive("UPDATE").await.respond(200, "OK").with_sdp(ANSWER).await;
    let answered = update.expect(200).await;
    assert_stack_contact(&answered, "a", s.b2bua.addr);

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// With no transfer feature the REFER relays to Alice; her 202 reaches Bob
/// under the Contact of Bob's face.
#[tokio::test(start_paused = true)]
async fn relayed_202_to_refer_states_the_stack_contact() {
    let h = Harness::new("relayed-202-refer-contact");
    let alice = h.agent("alice", "127.0.0.1:7113").await;
    let bob = h.agent("bob", "127.0.0.1:7114").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7114).start(&h, "b2bua", "127.0.0.1:7115").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", REFER_TO_CHARLIE)
        .send()
        .await;
    alice.receive("REFER").await.respond(202, "Accepted").await;
    let accepted = refer.expect(202).await;
    assert_stack_contact(&accepted, "b-1", b2bua.addr);

    let mut notify = alice_dialog
        .send_request(InDialogMethod::Notify)
        .with_header("Event", "refer")
        .with_header("Subscription-State", "terminated;reason=noresource")
        .with_header("Content-Type", "message/sipfrag;version=2.0")
        .with_sdp("SIP/2.0 200 OK\r\n")
        .send()
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    notify.expect(200).await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
