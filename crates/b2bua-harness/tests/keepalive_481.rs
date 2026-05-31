//! Keepalive where one peer answers 481 (port of `tests/scenarios/keepalive-481.ts`).
//!
//! Long call; the keepalive timer fires and the B2BUA sends an in-dialog OPTIONS
//! to *both* legs. Alice replies 200 OK; Bob replies 481 "Call/Transaction Does
//! Not Exist". The `handle-481` rule then:
//!   - terminate-leg(bob, bye_timeout)  — marks bob dead, suppresses the BYE to bob
//!   - add-cdr-event(bye, "Call/Transaction Does Not Exist")
//!   - begin-termination                — BYEs the responsive peer (alice) only
//!
//! Exercises `keepalive` + `absorb-options-200` + `handle-481`. The source backend
//! used a 15-min interval; the Rust default keepalive interval is 30 s, so we
//! advance in 30 s steps — the behaviour is identical.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use call::CdrEventType;
use scenario_harness::Harness;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn bob_481_on_options_byes_only_the_healthy_peer() {
    let h = Harness::new("b2bua-keepalive-481");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5086", "127.0.0.1", 5076).await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── Keepalive fires: alice 200, bob 481 ──────────────────────────────────
    h.advance(KEEPALIVE_INTERVAL).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(481, "Call/Transaction Does Not Exist").await;

    // ── handle-481 → begin-termination → BYE the responsive peer (alice) only ─
    // Bob's leg was marked dead (bye_timeout disposition) so no BYE is sent to bob.
    alice.receive("BYE").await.respond(200, "OK").await;

    // The CDR records the 481-driven teardown.
    for _ in 0..50 {
        if !b2bua.cdr_records().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the 481-terminated call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from 481 handling: {kinds:?}");

    // ── Assert the BYE was suppressed to bob (bye_timeout disposition) ───────
    // Exactly one BYE was put on the wire by the B2BUA — toward alice, the
    // responsive peer. Bob's dead leg must receive none.
    let report = h.finish().await;
    let bob_addr = bob.addr();
    let byes_to_bob = report
        .entries()
        .iter()
        .filter(|e| e.to == bob_addr)
        .filter(|e| {
            matches!(
                CustomParser::new().parse(&e.raw),
                Ok(SipMessage::Request(ref r)) if r.method == "BYE"
            )
        })
        .count();
    assert_eq!(byes_to_bob, 0, "handle-481 must suppress the BYE to the dead (481) peer");
}
