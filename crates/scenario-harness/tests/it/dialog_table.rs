//! Every simulated UA keeps an RFC 3261 §12 dialog table: a scripted receive
//! of a request on no dialog it holds fails at once, naming the UA, the
//! request and the tags it holds; a request the receive never pulls still
//! fails the run at `finish`; a `dialog-…` waiver on the UA opts a deliberate
//! one out, and a waiver that covers nothing is dead.

use scenario_harness::dialogs::{DIALOG_CSEQ_SKIP, DIALOG_UNKNOWN};
use scenario_harness::{Agent, Dialog, Harness, StepError, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

/// alice calls bob; returns her dialog and bob's.
async fn established(alice: &Agent, bob: &Agent) -> (Dialog, Dialog) {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    (dialog, uas.dialog())
}

/// bob sends alice an OPTIONS on their dialog, then a copy of it under the
/// From-tag `forged` and a fresh branch: the request a confused relay would
/// send. The audit's charge to bob for it is waived, being the stimulus.
async fn forged_options(
    h: &Harness,
    alice: &Agent,
    bob: &Agent,
    bob_dialog: &mut Dialog,
    forged: &str,
) {
    h.waive(
        WaiverScope::rule("in-dialog-from-tag", "bob forges the From-tag on purpose")
            .on_party("bob")
            .conditional(),
    );
    let (mut txn, sent) = bob_dialog
        .send_request(InDialogMethod::Options)
        .try_send_with_request()
        .await
        .expect("the OPTIONS leaves");
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    txn.expect(200).await;
    let text = String::from_utf8(sent.image().to_vec()).expect("text");
    let tag = sent.from().tag().expect("bob's tag").to_string();
    let wire = text.replacen(&format!("tag={tag}"), &format!("tag={forged}"), 1).replacen(
        "branch=z9hG4bK",
        "branch=z9hG4bK-forged",
        1,
    );
    assert!(matches!(CustomParser::new().parse(wire.as_bytes()), Ok(SipMessage::Request(_))));
    bob.try_send_datagram(wire.as_bytes(), alice.addr()).await.expect("the forged OPTIONS leaves");
}

/// A scripted receive of a request on no dialog fails at once, and its first
/// line says why: the UA, the method, the tag and the tags it holds.
#[tokio::test(start_paused = true)]
async fn a_scripted_receive_of_a_request_on_no_dialog_fails() {
    let h = Harness::new("dialog-table-receive");
    h.waive(
        WaiverScope::rule(DIALOG_UNKNOWN, "bob forges a From-tag on purpose").on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    // A test that opted in still drives the forged request; a run that did not
    // fails at this receive with the verdict below.
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    let mut got = alice.receive("OPTIONS").await;
    assert_eq!(got.request().from().tag(), Some("bob-forged"));
    let verdict = alice.dialog_verdict(got.request()).expect("the table judged it");
    let held = bob_dialog.local_tag().to_string();
    assert_eq!(
        verdict.to_string(),
        format!("OPTIONS From-tag bob-forged names no dialog (holds {held})")
    );
    assert_eq!(
        StepError::Dialog { who: "alice".into(), verdict: Box::new(verdict) }.to_string(),
        format!("alice: OPTIONS From-tag bob-forged names no dialog (holds {held})"),
    );
    got.respond(481, "Call/Transaction Does Not Exist").await;
    h.advance(std::time::Duration::from_millis(200)).await;
    assert!(bob.take_queued().await.is_some(), "the 481 reached bob");
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await;
}

/// Not opted in, the scripted receive itself errors with the verdict.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "alice: OPTIONS From-tag bob-forged names no dialog (holds bob-tag-")]
async fn the_receive_names_the_ua_the_request_and_the_tags_held() {
    let h = Harness::new("dialog-table-receive-fails");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (_alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    alice.receive("OPTIONS").await;
}

/// A request the body never pulls is still judged where it is sighted, and
/// the run fails at `finish`.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "red because dialog rule")]
async fn an_unpulled_request_on_no_dialog_fails_the_run_at_finish() {
    let h = Harness::new("dialog-table-finish");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    h.advance(std::time::Duration::from_millis(200)).await;
    assert_eq!(alice.drain_tu().await, 1, "the forged OPTIONS reached alice");
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await;
}

/// A `dialog-…` waiver that covers no verdict is dead, like any audit waiver.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "matched no finding")]
async fn a_dialog_waiver_that_covers_nothing_is_dead() {
    let h = Harness::new("dialog-table-dead-waiver");
    h.waive(WaiverScope::rule(DIALOG_CSEQ_SKIP, "nothing skips").on_party("alice"));
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut alice_dialog, _) = established(&alice, &bob).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await;
}

/// A waiver on another UA leaves the verdict gated.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "alice: OPTIONS From-tag bob-forged names no dialog")]
async fn a_waiver_on_another_ua_covers_nothing_here() {
    let h = Harness::new("dialog-table-other-party");
    h.waive(WaiverScope::rule(DIALOG_UNKNOWN, "wrong UA").on_party("bob").conditional());
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (_alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    alice.receive("OPTIONS").await;
}
