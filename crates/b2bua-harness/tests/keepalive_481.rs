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

use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn bob_481_on_options_byes_only_the_healthy_peer() {
    let s = B2buaScene::new("b2bua-keepalive-481").await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let _dialog = s.establish().await;

    // ── Keepalive fires: alice 200, bob 481 ──────────────────────────────────
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(481, "Call/Transaction Does Not Exist").await;

    // ── handle-481 → begin-termination → BYE the responsive peer (alice) only ─
    // Bob's leg was marked dead (bye_timeout disposition) so no BYE is sent to bob.
    s.alice.receive("BYE").await.respond(200, "OK").await;

    // The CDR records the 481-driven teardown.
    settle_until(|| !s.b2bua.cdr_records().is_empty()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the 481-terminated call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from 481 handling: {kinds:?}");

    // ── Assert the BYE was suppressed to bob (bye_timeout disposition) ───────
    // Exactly one BYE was put on the wire by the B2BUA — toward alice, the
    // responsive peer. Bob's dead leg must receive none.
    let bob_addr = s.bob.addr();
    let report = s.finish().await;
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
