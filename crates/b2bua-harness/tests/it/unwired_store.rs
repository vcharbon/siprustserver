//! A worker with no replication store wired writes nothing to its call store,
//! whatever the front proxy's stickiness cookie says. With two workers alive
//! the proxy names a backup peer on every INVITE (`w_bak`), the worker stamps
//! it onto the call's topology, and that topology alone must not open the
//! store: nothing reads what an unwired worker would write, and a delete the
//! writer drops would strand the body for good.
//!
//!   alice :5060 ──▶ proxy :5080 ──▶ w0 :5090 | w1 :5091 ──▶ proxy :5080 ──▶ bob :5070
//!
//! One full, properly terminated call; both workers' stores are read directly,
//! mid-call and after the teardown.

use std::sync::Arc;

use b2bua::store::InMemoryCallStore;
use b2bua_harness::{settle_until, spawn_proxy_core, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const W0: &str = "127.0.0.1:5090";
const W1: &str = "127.0.0.1:5091";

/// A worker behind the proxy with its own store handle retained, so the test
/// reads the store the core writes to.
async fn worker(h: &Harness, name: &str, addr: &str) -> (B2buaSut, Arc<InMemoryCallStore>) {
    let store = Arc::new(InMemoryCallStore::new());
    let sut = B2buaSut::route_all_to("127.0.0.1", 5070)
        .outbound_proxy("127.0.0.1", 5080)
        .with_store(store.clone())
        .start(h, name, addr)
        .await;
    (sut, store)
}

#[tokio::test(start_paused = true)]
async fn a_worker_with_no_replication_wired_writes_nothing_to_its_store() {
    let h = Harness::with_transit_delay("unwired-store-two-workers", 0).describe(
        "two workers behind the LB proxy, replication off on both: the cookie names a \
         backup peer, the call store stays empty for the whole call",
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let (ep, sock) = h.bind_sut("proxy", PROXY).await;
    let proxy = spawn_proxy_core(
        ep,
        sock,
        &[("w0", W0.parse().unwrap()), ("w1", W1.parse().unwrap())],
        Clock::test_at(0),
    );
    let (w0, w0_store) = worker(&h, "w0", W0).await;
    let (w1, w1_store) = worker(&h, "w1", W1).await;
    let stores = [("w0", &w0_store), ("w1", &w1_store)];

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Mid-call: the call is Active on one worker and has been through several
    // mutating turns. The writer is asynchronous, so let it drain before the
    // stores are read; an empty store then means nothing was submitted.
    settle_until(|| w0.metrics().creations_total() + w1.metrics().creations_total() == 1).await;
    h.advance(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        w0.metrics().creations_total() + w1.metrics().creations_total(),
        1,
        "the proxy placed the call on exactly one worker"
    );
    for (name, store) in stores {
        assert_eq!(
            store.lens(),
            (0, 0),
            "{name}: an unwired worker writes no body and no index key mid-call"
        );
    }

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let serving = if w0.metrics().creations_total() == 1 { &w0 } else { &w1 };
    settle_until(|| serving.metrics().removals_total() == serving.metrics().creations_total())
        .await;
    settle_until(|| serving.cdr_records().len() == 1).await;
    w0.assert_fully_reaped();
    w1.assert_fully_reaped();
    let cdrs = serving.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR per call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Answer), "answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "bye: {kinds:?}");

    h.advance(std::time::Duration::from_millis(100)).await;
    for (name, store) in stores {
        assert_eq!(store.lens(), (0, 0), "{name}: the store holds nothing after the call");
    }
    assert_eq!(
        w0.metrics().repl_flush_propagated_total() + w1.metrics().repl_flush_propagated_total(),
        0,
        "no flush counts as propagated with replication off"
    );

    let _report = h.finish().await;
}
