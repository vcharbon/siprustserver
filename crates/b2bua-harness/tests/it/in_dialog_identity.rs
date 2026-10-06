//! The From and To of every in-dialog request the back-to-back UA builds carry
//! the dialog's local and remote address as the dialog-creating INVITE stated
//! them, display name included. RFC 3261 §12.2.1.1 fixes the URIs and the tags;
//! the display name is the dialog's own, so it is not dropped on the way.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{B2buaScene, B2buaSut};
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;
use sip_message::SipRequest;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const ALICE_REOFFER: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
const BOB_REANSWER: &str = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";

/// The display names a request's From and To state.
fn displays(req: &SipRequest) -> (Option<String>, Option<String>) {
    (req.from().display().map(str::to_string), req.to().display().map(str::to_string))
}

fn named(from: &str, to: &str) -> (Option<String>, Option<String>) {
    (Some(from.to_string()), Some(to.to_string()))
}

/// On the caller's leg the stack is the UAS: toward her, From is the To she
/// called and To is her own From, both with the display names she wrote.
#[tokio::test(start_paused = true)]
async fn requests_toward_the_caller_keep_her_display_names() {
    let s = B2buaScene::new("b2bua-in-dialog-identity-caller").await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .from("\"Alice Caller\" <sip:alice@127.0.0.1:5060>")
        .to("\"Bob Callee\" <sip:bob@127.0.0.1:5070>")
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let toward_alice = named("Bob Callee", "Alice Caller");
    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut alice_reinv = s.alice.receive("INVITE").await;
    assert_eq!(displays(alice_reinv.request()), toward_alice, "re-INVITE");
    alice_reinv.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack(None).await;
    let ack = s.alice.receive("ACK").await;
    assert_eq!(displays(ack.request()), toward_alice, "ACK");

    let mut bye = bob_dialog.bye().await;
    let mut alice_bye = s.alice.receive("BYE").await;
    assert_eq!(displays(alice_bye.request()), toward_alice, "BYE");
    alice_bye.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = s.finish().await;
}

/// On a dialled leg the stack is the UAC: toward the callee, From and To are
/// the addresses its INVITE stated, display names the decision gave included.
#[tokio::test(start_paused = true)]
async fn requests_toward_the_callee_keep_the_dialled_display_names() {
    let s = B2buaScene::with_b2bua("b2bua-in-dialog-identity-callee", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await;
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": s.bob.addr().port()},
        "new_from": "\"Carol Caller\" <sip:+15551000@trunk.example>",
        "new_to": "\"Dave Callee\" <sip:+19005678@carrier.example>",
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_header("X-Api-Call", &plan)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let toward_bob = named("Carol Caller", "Dave Callee");
    let mut uas = s.bob.receive("INVITE").await;
    assert_eq!(displays(uas.request()), toward_bob, "the dialling INVITE");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let ack = s.bob.receive("ACK").await;
    assert_eq!(displays(ack.request()), toward_bob, "ACK");

    let mut reinv = dialog.request(InDialogMethod::Invite, Some(ALICE_REOFFER)).await;
    let mut bob_reinv = s.bob.receive("INVITE").await;
    assert_eq!(displays(bob_reinv.request()), toward_bob, "re-INVITE");
    bob_reinv.respond(200, "OK").with_sdp(BOB_REANSWER).await;
    reinv.expect(200).await;
    dialog.ack(None).await;
    let reack = s.bob.receive("ACK").await;
    assert_eq!(displays(reack.request()), toward_bob, "re-INVITE ACK");

    let mut bye = dialog.bye().await;
    let mut bob_bye = s.bob.receive("BYE").await;
    assert_eq!(displays(bob_bye.request()), toward_bob, "BYE");
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = s.finish().await;
}

/// A header parameter on the caller's From other than the tag stays with her
/// address: the BYE toward her states it in To beside the dialog's tag.
#[tokio::test(start_paused = true)]
async fn a_from_header_parameter_rides_the_requests_toward_the_caller() {
    let s = B2buaScene::new("b2bua-in-dialog-identity-param").await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .from("\"Alice Caller\" <sip:alice@127.0.0.1:5060>;x-param=1")
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye = uas.dialog().bye().await;
    let mut alice_bye = s.alice.receive("BYE").await;
    let to = alice_bye.request().to().clone();
    assert_eq!(to.display(), Some("Alice Caller"));
    assert!(to.param("x-param").is_some(), "the header parameter rides: {to}");
    assert!(to.tag().is_some_and(|t| !t.is_empty()), "beside the dialog's tag: {to}");
    assert_eq!(to.params().iter().filter(|(k, _)| k.eq_ignore_ascii_case("tag")).count(), 1);
    alice_bye.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = s.finish().await;
}

/// A second early dialog forked off the dialled INVITE takes the same
/// addresses as the first: its PRACK, its ACK and its BYE keep the display
/// names the decision gave.
#[tokio::test(start_paused = true)]
async fn a_second_forked_early_dialog_keeps_the_dialled_display_names() {
    let s = B2buaScene::with_b2bua("b2bua-in-dialog-identity-fork", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await;
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": s.bob.addr().port()},
        "new_from": "\"Carol Caller\" <sip:+15551000@trunk.example>",
        "new_to": "\"Dave Callee\" <sip:+19005678@carrier.example>",
    })
    .to_string();
    let toward_bob = named("Carol Caller", "Dave Callee");
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_header("X-Api-Call", &plan)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;

    let mut fork_tags = Vec::new();
    for (fork, rseq) in [("bobfork1", "1"), ("bobfork2", "1")] {
        uas.respond(183, "Session Progress")
            .with_to_tag(fork)
            .with_header("Require", "100rel")
            .with_header("RSeq", rseq)
            .with_sdp(ANSWER)
            .await;
        let p = call.expect(183).await;
        let a_tag = p.to().tag().expect("an a-facing tag").to_string();
        let rack =
            format!("{} 1 INVITE", p.header::<RSeq>().expect("RSeq").expect("readable").value());
        let mut prack = call
            .send_request(InDialogMethod::Prack)
            .with_to_tag(&a_tag)
            .with_rack(&rack)
            .send()
            .await;
        let mut at_bob = s.bob.receive("PRACK").await;
        assert_eq!(at_bob.request().to().tag(), Some(fork));
        assert_eq!(displays(at_bob.request()), toward_bob, "PRACK on {fork}");
        at_bob.respond(200, "OK").await;
        prack.expect(200).await;
        fork_tags.push(a_tag);
    }

    uas.respond(200, "OK").with_to_tag("bobfork2").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let ack = s.bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("bobfork2"));
    assert_eq!(displays(ack.request()), toward_bob, "ACK on the second fork");

    let mut bye = dialog.bye().await;
    let mut bob_bye = s.bob.receive("BYE").await;
    assert_eq!(displays(bob_bye.request()), toward_bob, "BYE on the second fork");
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = s.finish().await;
}
