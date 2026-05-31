//! End-to-end failure paths: a b-leg rejection is relayed to the caller and the
//! call terminates; a decision-engine reject answers the caller directly.

use std::sync::Arc;

use b2bua::decision::test_adapter::reject;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::B2buaSut;
use call::CdrEventType;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=a 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

#[tokio::test]
async fn b_leg_busy_is_relayed_and_call_terminates() {
    let h = Harness::with_transit_delay("b2bua-busy", 0);
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5081", "127.0.0.1", 5071).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    call.expect(486).await; // the 486 is relayed back to alice

    for _ in 0..50 {
        if !b2bua.cdr_records().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the rejected call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Reject), "reject event: {kinds:?}");

    let _r = h.finish().await;
}

#[tokio::test]
async fn decision_reject_answers_caller_directly() {
    let h = Harness::with_transit_delay("b2bua-reject", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| reject(403, "Forbidden"))
            .build(),
    );
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5082", decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    call.expect(403).await; // rejected without ever contacting bob

    for _ in 0..50 {
        if !b2bua.cdr_records().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1);
    assert!(cdrs[0].b_legs.is_empty(), "no b-leg created on reject");

    let _r = h.finish().await;
}
