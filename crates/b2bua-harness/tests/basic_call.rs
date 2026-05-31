//! End-to-end: alice ↔ b2bua ↔ bob. INVITE → 180 → 200 → ACK → BYE, with the
//! B2BUA bridging two independent dialogs. Asserts the call establishes and
//! tears down and that exactly one CDR (with answer + bye) is produced.

use call::CdrEventType;
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test]
async fn alice_calls_bob_through_b2bua() {
    let h = Harness::with_transit_delay("b2bua-basic", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5080", "127.0.0.1", 5070).await;

    // alice INVITEs (addressed to bob) but sends through the B2BUA.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    // The B2BUA bridges to bob.
    let mut uas = bob.receive("INVITE").await;
    assert!(!uas.request().body.is_empty(), "offer relayed to bob");

    uas.respond(180, "Ringing").await;
    call.expect(180).await; // 100 Trying from the txn layer is absorbed by the UAC

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "answer relayed to alice");

    // ACK is relayed end-to-end.
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // alice hangs up; the B2BUA answers her and BYEs bob.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // Give the worker tasks a moment to drain the final teardown + CDR.
    for _ in 0..50 {
        if b2bua.cdr_records().len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR per call");
    let cdr = &cdrs[0];
    let kinds: Vec<CdrEventType> = cdr.events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::InviteReceived), "invite_received: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Answer), "answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "bye: {kinds:?}");
    assert_eq!(cdr.b_legs.len(), 1, "one b-leg");

    let _report = h.finish().await;
}
