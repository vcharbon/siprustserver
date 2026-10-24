//! The call store is written only when a replication store is wired. A call
//! whose topology names a backup peer (the front proxy's stickiness cookie is
//! stamped whenever two workers are alive) still flushes and deletes nothing
//! on a node with no replication store: nothing reads what such a node would
//! write, and the node holds no writer to write it with.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use call::Call;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::{CallState, CallStore, InMemoryCallStore, PartitionRole, PutOpts, StoreError};
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

/// A `CallState` with no replication store, over `store`: the wiring of a
/// node whose replication switch is off.
fn unwired_state(store: &Arc<InMemoryCallStore>, metrics: &B2buaMetrics) -> CallState {
    CallState::new(store.clone() as Arc<dyn CallStore>, "w0", metrics.clone())
}

fn cookie_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let src = SocketAddr::from(([10, 0, 0, 9], 5060));
    build_initial_call(
        &invite_with_cookie("w0", "w1"),
        src,
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    )
}

/// With no replication store wired, a call that names a backup peer is created,
/// mutated and flushed over several turns, then removed: the store holds no body
/// and no index key at any point, no flush counts as propagated, and the state
/// holds no writer that could have written.
#[tokio::test(start_paused = true)]
async fn a_store_with_no_replication_wired_holds_nothing_after_a_call() {
    let store = Arc::new(InMemoryCallStore::new());
    let metrics = B2buaMetrics::new();
    let state = unwired_state(&store, &metrics);
    assert!(state.repl_store().is_none(), "an unwired state holds no store write path");

    let call = cookie_call();
    let call_ref = call.call_ref.clone();
    assert_eq!(
        crate::router::topology_cookie::placement(&call).map(|t| t.bak),
        Some("w1".to_string()),
        "the cookie stamps a backup peer; the topology alone must not open the store"
    );
    state.create_placed(call);

    // Three authoritative turns, each flushed the way the router flushes an
    // Active call after a non-quiet turn (the pre-bump clone is what it passes).
    for _ in 0..3 {
        let before = state.peek(&call_ref).unwrap();
        state.update(before.clone());
        state.flush(&before);
    }
    assert_eq!(
        store.lens(),
        (0, 0),
        "a flush with no replication store wired writes nothing (bodies, indexes)"
    );
    assert_eq!(metrics.repl_flush_propagated_total(), 0, "nothing propagated on the off path");

    state.remove(&call_ref);
    assert!(state.peek(&call_ref).is_none(), "remove still evicts the in-memory call");
    assert_eq!(store.lens(), (0, 0), "the store holds nothing after the call");
}

/// A store that counts the writes reaching it, over an in-memory store.
#[derive(Default)]
struct CountingStore {
    inner: InMemoryCallStore,
    puts: AtomicUsize,
    deletes: AtomicUsize,
}

#[async_trait]
impl CallStore for CountingStore {
    async fn get_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Result<Option<Arc<[u8]>>, StoreError> {
        self.inner.get_call(role, primary, call_ref).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        body: Vec<u8>,
        indexes: &[String],
        ttl_ms: i64,
        call_gen: i64,
        call_bgen: i64,
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner
            .put_call(role, primary, call_ref, body, indexes, ttl_ms, call_gen, call_bgen, opts)
            .await
    }

    async fn delete_call(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
        indexes: &[String],
        answered: bool,
        opts: &PutOpts,
    ) -> Result<(), StoreError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete_call(role, primary, call_ref, indexes, answered, opts).await
    }

    async fn get_index(&self, index_key: &str) -> Result<Option<String>, StoreError> {
        self.inner.get_index(index_key).await
    }

    async fn scan_calls(
        &self,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.inner.scan_calls(role, primary).await
    }
}

/// The delete is gated on its own, not only invisible behind a gated flush: with
/// no replication store wired, a create → update → flush → remove sends the
/// store neither a put nor a delete. A store that holds nothing cannot show a
/// suppressed delete, so the writes are counted at the store instead.
#[tokio::test(start_paused = true)]
async fn no_replication_wired_sends_the_store_neither_put_nor_delete() {
    let store = Arc::new(CountingStore::default());
    let state = CallState::new(store.clone() as Arc<dyn CallStore>, "w0", B2buaMetrics::new());

    let call = cookie_call();
    let call_ref = call.call_ref.clone();
    state.create_placed(call);
    let before = state.peek(&call_ref).unwrap();
    state.update(before.clone());
    state.flush(&before);
    state.remove(&call_ref);

    assert_eq!(store.puts.load(Ordering::SeqCst), 0, "no put reaches the store");
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "no delete reaches the store");
}

/// The state stamp is set where the vector is bumped (ADR-0055 §7): a write
/// that changes only exempt fields moves the vector and leaves the stamp; a
/// write that changes a compared field stamps the vector it bumps.
#[test]
fn a_write_stamps_the_vector_only_when_it_changes_a_compared_field() {
    let store = Arc::new(InMemoryCallStore::new());
    let state = unwired_state(&store, &B2buaMetrics::new());
    let call = cookie_call();
    let call_ref = state.create_placed(call.clone());
    let versions = || {
        let t = state.topology(&call_ref).expect("a proxied call");
        (t.vector(), t.stamp())
    };
    let start = versions();

    let mut exempt = state.peek(&call_ref).expect("resident");
    exempt.message_count = Some(7);
    exempt.timer_generation += 3;
    state.update(exempt);
    let (vector, stamp) = versions();
    assert_eq!(vector, (start.0 .0 + 1, start.0 .1), "the write is counted");
    assert_eq!(stamp, start.1, "an exempt-only write leaves the stamp");

    let mut compared = state.peek(&call_ref).expect("resident");
    compared.callback_context = Some("a compared field".into());
    state.update(compared);
    let (vector, stamp) = versions();
    assert_eq!(stamp, vector, "a compared write stamps the vector it bumps");
    assert_eq!(vector, (start.0 .0 + 2, start.0 .1));
}
