//! A callee BYE on one early fork (RFC 3261 §15: a callee MUST NOT BYE an
//! early dialog; the deviation under test) ends the call while the leg's
//! INVITE still rings on its other fork. That INVITE has no final and no
//! CANCEL: the teardown answers the caller at once and CANCELs it (§9.1);
//! the record is released once its 487 completes it, or, with the callee
//! silent, once its transaction is given up.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

#[tokio::test(start_paused = true)]
async fn an_early_fork_bye_cancels_the_invite_its_sibling_still_rings_on() {
    let h = Harness::with_transit_delay("early-fork-bye", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    call.expect(180).await;

    h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "the callee's BYE on its early dialog is the deviation under test (RFC 3261 §15)",
    );
    uas.adopt_to_tag("fork1");
    let mut fork1 = uas.dialog();
    fork1.send_request(InDialogMethod::Bye).send().await.expect(200).await;
    // The caller is answered at once: she is owed nothing from the CANCEL.
    call.expect(503).await;

    let mut cancel = bob.receive("CANCEL").await;
    h.advance(Duration::from_secs(2)).await;
    assert!(!b2bua.is_reaped(), "the record waits for the ringing INVITE's final");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The callee never answers the CANCEL: the record is still released, once
/// the CANCEL's transaction gives up and the terminating timeout passes.
#[tokio::test(start_paused = true)]
async fn a_silent_callee_does_not_hold_the_record_past_the_terminating_timeout() {
    let h = Harness::with_transit_delay("early-fork-bye-silent", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    call.expect(180).await;

    h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "the callee's BYE on its early dialog is the deviation under test (RFC 3261 §15)",
    );
    uas.adopt_to_tag("fork1");
    let mut fork1 = uas.dialog();
    fork1.send_request(InDialogMethod::Bye).send().await.expect(200).await;
    call.expect(503).await;
    bob.receive("CANCEL").await;

    h.advance(Duration::from_secs(40)).await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The callee's UPDATE on its other fork is relayed to the caller, who has
/// not answered it when the early BYE ends the call. Her INVITE takes its
/// final at once, which ends her leg: the callee's UPDATE draws its 481 then
/// (RFC 3261 §8.2.6, ADR-0055 §2), not its own Timer F.
#[tokio::test(start_paused = true)]
async fn a_request_relayed_to_the_unanswered_caller_is_answered_with_her_final() {
    let h = Harness::with_transit_delay("early-fork-bye-relayed-update", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    call.expect(180).await;

    uas.adopt_to_tag("fork2");
    let mut fork2 = uas.dialog();
    let mut update = fork2.request(InDialogMethod::Update, None).await;
    let mut at_alice = alice.receive("UPDATE").await;

    h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "the callee's BYE on its early dialog is the deviation under test (RFC 3261 §15)",
    );
    uas.adopt_to_tag("fork1");
    let mut fork1 = uas.dialog();
    fork1.send_request(InDialogMethod::Bye).send().await.expect(200).await;
    call.expect(503).await;
    let mut cancel = bob.receive("CANCEL").await;
    update.expect(481).await;
    // Her early dialog ended with her INVITE's 503.
    at_alice.respond(481, "Call/Transaction Does Not Exist").await;

    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
