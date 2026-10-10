//! A callee BYE on one early fork (RFC 3261 §15: a callee MUST NOT BYE an
//! early dialog; the deviation under test) ends the call while the leg's
//! INVITE still rings on its other fork. That INVITE has no final and no
//! CANCEL: the teardown CANCELs it (§9.1), and the call is released, its
//! caller answered, only once its 487 completes it.

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

    let mut cancel = bob.receive("CANCEL").await;
    h.advance(Duration::from_secs(2)).await;
    assert!(!b2bua.is_reaped(), "the record waits for the ringing INVITE's final");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    // The caller is answered as the call ends (ADR-0022).
    call.expect(503).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
