//! The call store is written only when a replication store is wired. A call
//! whose topology names a backup peer (the front proxy's stickiness cookie is
//! stamped whenever two workers are alive) still flushes and deletes nothing
//! on a node with no replication store: nothing reads what such a node would
//! write, and a dropped delete would strand the body for good.

use std::net::SocketAddr;
use std::sync::Arc;

use call::Call;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::{BufferedTerminateWriter, CallState, CallStore, InMemoryCallStore};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;

/// An INVITE carrying the proxy's stickiness cookie (`w_pri`/`w_bak`) as URI
/// params on its Record-Route, the shape a worker behind the front proxy sees.
fn invite_with_cookie(pri: &str, bak: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-unwired\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: call-unwired@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A `CallState` with no replication store, over `store`, draining to it
/// through a writer of `capacity` slots: the wiring of a node whose
/// replication switch is off.
fn unwired_state(
    store: &Arc<InMemoryCallStore>,
    capacity: usize,
    metrics: &B2buaMetrics,
) -> CallState {
    let writer = BufferedTerminateWriter::spawn(store.clone() as Arc<dyn CallStore>, capacity);
    CallState::new(store.clone() as Arc<dyn CallStore>, writer, "w0", metrics.clone())
}

fn cookie_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let src = SocketAddr::from(([10, 0, 0, 9], 5060));
    build_initial_call(&invite_with_cookie("w0", "w1"), src, &config, 0)
}

/// Let the writer's drainer task run. The writer is asynchronous: under the
/// paused current-thread runtime every submitted op is applied to the store
/// once the drainer is polled, which a handful of yields guarantees. The
/// assertions below read the store only after this.
async fn drain() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// With no replication store wired, a call that names a backup peer is created,
/// mutated and flushed over several turns, then removed: the store holds no body
/// and no index key at any point, and no flush counts as propagated.
#[tokio::test(start_paused = true)]
async fn a_store_with_no_replication_wired_holds_nothing_after_a_call() {
    let store = Arc::new(InMemoryCallStore::new());
    let metrics = B2buaMetrics::new();
    let state = unwired_state(&store, 16, &metrics);

    let call = cookie_call();
    let call_ref = call.call_ref.clone();
    assert_eq!(
        call.topology.as_ref().map(|t| t.bak.as_str()),
        Some("w1"),
        "the cookie stamps a backup peer; the topology alone must not open the store"
    );
    state.create(call);

    // Three authoritative turns, each flushed the way the router flushes an
    // Active call after a non-quiet turn (the pre-bump clone is what it passes).
    for _ in 0..3 {
        let before = state.peek(&call_ref).unwrap();
        state.update(before.clone());
        state.flush(&before);
    }
    drain().await;
    assert_eq!(
        store.lens(),
        (0, 0),
        "a flush with no replication store wired writes nothing (bodies, indexes)"
    );
    assert_eq!(metrics.repl_flush_propagated_total(), 0, "nothing propagated on the off path");

    state.remove(&call_ref);
    drain().await;
    assert!(state.peek(&call_ref).is_none(), "remove still evicts the in-memory call");
    assert_eq!(store.lens(), (0, 0), "the store holds nothing after the call");
}

/// The harm a write on the off path does: the writer drops on a full channel,
/// so a delete submitted behind an undrained flush is lost and the body it would
/// have removed stays forever (no reaper runs without a replication store). With
/// no write at all there is nothing to strand. The put and the delete are
/// submitted with no yield between them, so a one-slot writer sees them
/// back-to-back whatever the runtime's scheduling.
#[tokio::test(start_paused = true)]
async fn a_dropped_delete_strands_nothing_when_no_replication_is_wired() {
    let store = Arc::new(InMemoryCallStore::new());
    let metrics = B2buaMetrics::new();
    let state = unwired_state(&store, 1, &metrics);

    let call = cookie_call();
    let call_ref = call.call_ref.clone();
    state.create(call);
    let before = state.peek(&call_ref).unwrap();
    state.update(before.clone());
    state.flush(&before);
    state.remove(&call_ref);
    drain().await;

    assert!(state.peek(&call_ref).is_none(), "remove evicts the in-memory call");
    assert_eq!(store.lens(), (0, 0), "no body is stranded in a store nothing reads");
}
