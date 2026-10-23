//! A relayed in-dialog request whose target never answers.
//!
//! The relay's client transaction times out (Timer F, RFC 3261 §17.1.2.2).
//! A transaction timeout affects only the usage it occurred in (RFC 5057
//! §5.2), and a relayed request belongs to the peers' exchange: the relay's
//! record is settled, the session and both dialogs stay up. A
//! transaction-stateful element sends no 408 to a non-INVITE request (RFC
//! 4320 §4.2), so the originator's own Timer F ends its transaction.

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
const ORDINAL: &str = "w0";

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

/// The caller's INFO is relayed to the callee, who never answers it: past
/// Timer F the call is still up with no BYE on either side and no final sent
/// to the caller; her BYE then ends the call normally.
#[tokio::test(start_paused = true)]
async fn a_relayed_info_the_target_never_answers_leaves_the_call_up() {
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
    let call_ref = call::derive_call_ref(ORDINAL, &call.call_id(), alice_dialog.local_tag());

    // The callee takes the INFO and never answers it.
    let _info = alice_dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await;

    // Past Timer F (64·T1 = 32 s) of the relay and of the caller's own INFO.
    h.advance(Duration::from_secs(33)).await;
    bob.drain().await; // the relay's retransmissions
    alice.drain().await;
    assert_eq!(b2bua.active_calls(), 1, "the relay's timeout leaves the call up");
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert!(
        live.b_legs.iter().all(|l| l.end().is_none()) && live.a_leg.end().is_none(),
        "no leg is ended by the relay's timeout",
    );
    assert!(
        responses_toward(&h, alice.addr(), "INFO").is_empty(),
        "the caller's INFO draws no final from this stack (RFC 4320 §4.2)",
    );
    assert!(
        !h.snapshot_report().entries().iter().any(|e| e.raw.starts_with(b"BYE ")),
        "no BYE leaves on either side",
    );

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}
