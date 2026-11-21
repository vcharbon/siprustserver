//! A relayed in-dialog request whose target never answers.
//!
//! The relay's client transaction times out (Timer F, RFC 3261 §17.1.2.2).
//! The timeout is read as an own request's would be: on a leg that carries
//! the call, the dialog that did not answer is terminated (RFC 3261
//! §12.2.1.2) and the call with it. A transaction-stateful element sends no
//! 408 to a non-INVITE request (RFC 4320 §4.2), so the originator's own
//! Timer F ends its transaction.

use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The responses put on the wire toward `to` answering `method`.
fn responses_toward(h: &Harness, to: std::net::SocketAddr, method: &str) -> Vec<u16> {
    let parser = CustomParser::new();
    h.snapshot_report()
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) if r.cseq().method().as_str() == method => Some(r.status()),
            _ => None,
        })
        .collect()
}

/// The caller's INFO is relayed to the callee, who never answers it. The
/// callee's leg carries the call, so past the relay's Timer F its dialog is
/// terminated (RFC 3261 §12.2.1.2) and the call with it: both sides are
/// BYE'd, and the caller's INFO draws no final from this stack.
#[tokio::test(start_paused = true)]
async fn a_relayed_info_the_target_never_answers_ends_the_call() {
    let h = Harness::new("relayed-request-timeout");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = std::sync::Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        // No keepalive inside the Timer F window.
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // The callee takes the INFO and never answers it.
    let _info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    let _unanswered = bob.receive("INFO").await;

    // Past Timer F (64·T1 = 32 s) of the relay and of the caller's own INFO.
    h.advance(Duration::from_secs(33)).await;
    bob.drain().await; // the relay's retransmissions
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    assert!(
        responses_toward(&h, alice.addr(), "INFO").is_empty(),
        "the caller's INFO draws no final from this stack (RFC 4320 §4.2)",
    );
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}
