//! Every simulated UA keeps an RFC 3261 §12 dialog table: a scripted receive
//! of a request on no dialog it holds fails at once, naming the UA, the
//! request and the tags it holds; a request the receive never pulls still
//! fails the run at `finish`; a `dialog-…` waiver on the UA opts a deliberate
//! one out, and a waiver that covers nothing is dead.

use scenario_harness::dialogs::{DIALOG_CSEQ_SKIP, DIALOG_UNKNOWN};
use scenario_harness::{
    Agent, ClientInvite, Dialog, Harness, InDialogTxn, ServerTxn, StepError, WaiverScope,
};
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
    let report = h.finish().await;
    assert_eq!(
        report.waived_sut_findings().get(DIALOG_UNKNOWN),
        Some(&1),
        "a waived dialog verdict counts as a waived SUT finding"
    );
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

/// A request no step pulls is still judged: `finish` sights what each UA left
/// queued.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "dialog rule dialog-unknown fired on alice")]
async fn a_request_left_queued_fails_the_run_at_finish() {
    let h = Harness::new("dialog-table-queued");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (_alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    h.finish().await;
}

/// A collected dialog verdict charges the sender of the message it judges,
/// by the source address the UA read it from.
#[tokio::test(start_paused = true)]
async fn a_collected_dialog_verdict_charges_its_messages_sender() {
    let h = Harness::new("dialog-table-charged");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    h.advance(std::time::Duration::from_millis(200)).await;
    assert_eq!(alice.drain_tu().await, 1, "the forged OPTIONS reached alice");
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let (_report, findings) = h.finish_collecting().await;
    let charged: Vec<(&str, Option<&str>)> = findings
        .iter()
        .filter(|f| f.rule == DIALOG_UNKNOWN)
        .map(|f| (f.lane.as_str(), f.charged.as_deref()))
        .collect();
    assert_eq!(charged, [("alice", Some("127.0.0.1:5070"))]);
}

/// A request `drain` discards is sighted, and a harness dropped without
/// `finish` still fails on its verdict.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "dialog rule dialog-unknown fired on alice")]
async fn a_drained_request_fails_a_harness_dropped_without_finish() {
    let h = Harness::new("dialog-table-drop");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (_alice_dialog, mut bob_dialog) = established(&alice, &bob).await;
    forged_options(&h, &alice, &bob, &mut bob_dialog, "bob-forged").await;
    h.advance(std::time::Duration::from_millis(200)).await;
    assert_eq!(alice.drain().await, 1);
    drop(h);
}

/// alice calls bob, who rings and sends her an INFO on the early dialog,
/// which she takes; then bob refuses the call 487. Returns her INVITE, bob's
/// server transaction, the INFO as alice holds it and bob's INFO transaction.
async fn refused_after_an_early_info(
    alice: &Agent,
    bob: &Agent,
) -> (ClientInvite, ServerTxn, ServerTxn, InDialogTxn) {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").send().await;
    call.expect(180).await;
    let info = uas.dialog().send_request(InDialogMethod::Info).send().await;
    let at_alice = alice.receive("INFO").await;
    uas.respond(487, "Request Terminated").await;
    (call, uas, at_alice, info)
}

/// RFC 3261 §17.2.1: alice answers bob's early INFO before his 487 reaches
/// her, so her 200 crosses his final and is no verdict at bob.
#[tokio::test(start_paused = true)]
async fn a_2xx_crossing_the_callees_final_passes() {
    let h = Harness::new("dialog-table-crossing-final");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut call, uas, mut at_alice, mut info) = refused_after_an_early_info(&alice, &bob).await;
    at_alice.respond(200, "OK").await;
    info.expect(200).await;
    call.expect(487).await;
    uas.expect_ack().await;
    h.finish().await;
}

/// RFC 3261 §12.3, §9.2: alice took bob's early INFO before his 487 reached
/// her, so her 200 to it is no verdict at bob though it follows her ACK of
/// that final.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_an_info_taken_before_the_callees_final_passes_after_its_ack() {
    let h = Harness::new("dialog-table-answered-after-ack");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut call, uas, mut at_alice, mut info) = refused_after_an_early_info(&alice, &bob).await;
    call.expect(487).await;
    uas.expect_ack().await;
    at_alice.respond(200, "OK").await;
    info.expect(200).await;
    h.finish().await;
}

/// alice calls bob, who rings, then refuses the call 487. Returns her INVITE
/// and bob's server transaction.
async fn refused_after_ringing(alice: &Agent, bob: &Agent) -> (ClientInvite, ServerTxn) {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").send().await;
    call.expect(180).await;
    uas.respond(487, "Request Terminated").await;
    (call, uas)
}

/// RFC 3261 §12.3, §17.2.1: alice sends bob an INFO on the early dialog
/// before his 487 reaches her. Bob owes it a 481, and it is no verdict.
#[tokio::test(start_paused = true)]
async fn a_request_crossing_the_callees_final_is_refused_and_passes() {
    let h = Harness::new("dialog-table-request-crossing-final");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut call, uas) = refused_after_ringing(&alice, &bob).await;
    let mut info = call.send_request(InDialogMethod::Info).send().await;
    let mut at_bob = bob.receive("INFO").await;
    let refusal = bob.dialog_verdict(at_bob.request()).and_then(|v| v.kind.refusal());
    assert_eq!(refusal, Some((481, "Call/Transaction Does Not Exist")));
    at_bob.respond(481, "Call/Transaction Does Not Exist").await;
    info.expect(481).await;
    call.expect(487).await;
    uas.expect_ack().await;
    h.finish().await;
}

/// alice sends bob an INFO on the early dialog after she ACKed his 487: she
/// knew the dialog ended, and the INFO is a verdict at bob.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "names a dialog a 487 final ended")]
async fn a_request_after_the_ack_of_the_callees_final_fails() {
    let h = Harness::new("dialog-table-request-after-ack");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut call, uas) = refused_after_ringing(&alice, &bob).await;
    call.expect(487).await;
    uas.expect_ack().await;
    call.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await;
}

/// A `dialog-…` waiver naming no UA covers nothing: every dialog verdict is a
/// finding against the SUT, so it is a dead waiver, conditional or not.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "matched no finding")]
async fn a_dialog_waiver_naming_no_ua_is_dead() {
    let h = Harness::new("dialog-table-partyless");
    h.waive(WaiverScope::rule(DIALOG_UNKNOWN, "no UA named").conditional());
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (mut alice_dialog, _) = established(&alice, &bob).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await;
}
