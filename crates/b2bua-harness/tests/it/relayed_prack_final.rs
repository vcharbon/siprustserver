//! A relayed PRACK the far leg does not answer 2xx.
//!
//! The caller's PRACK names the reliable provisional this stack showed her, so
//! this stack, the UAS of her leg, owes it a 2xx (RFC 3262 §3) whatever the far
//! leg makes of the relayed copy: a failure final there, or no final at all
//! before the relay's Timer F (RFC 3261 §17.1.2.2), draws the caller's 200.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::dialogs::{DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP};
use scenario_harness::{Agent, Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{Arrival, PreIngressAction};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Bob's own `RSeq`.
const BOB_RSEQ: u32 = 4711;

/// A callee every PRACK reaching which is lost while `lost` holds.
async fn callee_losing_pracks(h: &Harness, lost: Arc<AtomicBool>) -> Agent {
    h.agent_with_pre_ingress(
        "bob",
        &h.next_addr().to_string(),
        Arc::new(move |arrival: &mut Arrival, _src, _load| {
            if arrival.raw().starts_with(b"PRACK ") && lost.load(Ordering::SeqCst) {
                return PreIngressAction::Drop;
            }
            PreIngressAction::Accept
        }),
    )
    .await
}

/// The statuses put on the wire toward `to` answering `method`.
fn responses_toward(h: &Harness, to: SocketAddr, method: &str) -> Vec<u16> {
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

/// The relayed PRACK is lost toward bob, the caller's early UPDATE overtakes
/// it there, and its retransmission arrives with a CSeq gone backwards: bob
/// answers it 500 (RFC 3261 §12.2.2). The caller's PRACK draws 200, the call
/// is answered and ends normally.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_refused_by_the_callee_draws_the_caller_s_200() {
    let h = Harness::with_transit_delay("relayed-prack-refused", 1);
    h.waive(
        WaiverScope::rule(DIALOG_CSEQ_SKIP, "the PRACK lost toward bob is the stimulus")
            .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            DIALOG_CSEQ_BACKWARDS,
            "the PRACK's retransmission reaches bob after the UPDATE: its 500 is the stimulus",
        )
        .on_party("bob"),
    );
    let lost = Arc::new(AtomicBool::new(true));
    let alice = h.agent_auto("alice").await;
    let bob = callee_losing_pracks(&h, lost.clone()).await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");

    let mut update = call.send_request(InDialogMethod::Update).send().await;
    bob.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;

    lost.store(false, Ordering::SeqCst);
    bob.receive("PRACK").await.respond(500, "Server Internal Error").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Every copy of the relayed PRACK is lost toward bob, who answers the INVITE
/// meanwhile. When the relay's transaction times out, the caller's PRACK is
/// answered 200; the call stays up and ends normally.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_that_times_out_draws_the_caller_s_200() {
    let h = Harness::with_transit_delay("relayed-prack-timeout", 1);
    h.waive(
        WaiverScope::rule(DIALOG_CSEQ_SKIP, "the PRACK lost toward bob is the stimulus")
            .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let bob = callee_losing_pracks(&h, Arc::new(AtomicBool::new(true))).await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).await;
    let p183 = call.expect(183).await;
    let _prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(33)).await;
    alice.drain().await;
    assert_eq!(
        responses_toward(&h, alice.addr(), "PRACK"),
        vec![200],
        "the caller's PRACK draws one 200 once the relayed copy times out (RFC 3262 §3)",
    );
    assert_eq!(b2bua.active_calls(), 1, "the relay's timeout leaves the call up");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
