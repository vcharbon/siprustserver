//! S11 (ADR-0011 X11) fail-back tests — the `CallState`-level mechanics the
//! active-reclaim + `Deactivate` handshake is built on:
//!
//! 1. **takeover tagging + handback targeting** — `mark_takeover` stamps an
//!    acting-backup copy's activation instant; `deactivate_targets` selects the
//!    copies a `Deactivate{since T}` must shed (scoped to the asking primary,
//!    bounded by `as_of`);
//! 2. **local-only handback** — `drop_local` sheds the live copy WITHOUT
//!    propagating a delete: the backup `Element` survives (the call lives on at
//!    its reclaiming primary);
//! 3. **active reclaim read-paths** — `reclaim_scan` (bulk) + `peek_reclaimable`
//!    (reactive straggler) decode this node's `pri:` partition, and
//!    `materialize_if_absent` inserts idempotently.
//!
//! These exercise the seams directly (no full SIP failover harness — that is the
//! cluster `chaos.sh bringback` acceptance).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallBodyCodec, MsgpackCodec};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::ReplicatingCallStore;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::store::{
    BufferedTerminateWriter, CallState, CallStore, InMemoryCallStore, PartitionRole, PutOpts,
};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn config_for(ordinal: &str) -> B2buaConfig {
    B2buaConfig {
        self_ordinal: ordinal.into(),
        ..Default::default()
    }
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A proxied INVITE carrying the `w_pri`/`w_bak` stickiness cookie, parametrised
/// by Call-ID so each call gets a distinct `callRef` (`{pri}|{cid}|alicetag`).
fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A `CallState` for `ordinal` wired to `repl` as its replicating store (mirrors
/// how `B2buaCore` builds it: an in-memory `store` + the replicating one).
fn call_state(ordinal: &str, repl: Arc<ReplicatingCallStore>) -> CallState {
    let store = Arc::new(InMemoryCallStore::new()) as Arc<dyn CallStore>;
    let writer = BufferedTerminateWriter::spawn(store.clone(), 1024);
    CallState::new(store, writer, ordinal, B2buaMetrics::new()).with_replication(repl)
}

/// Seed a call body into `(role, primary)` of the replicating store.
async fn put(store: &ReplicatingCallStore, role: PartitionRole, primary: &str, call: &Call) {
    let body = MsgpackCodec::new().encode(call);
    let gen = call.topology.as_ref().map(|t| t.gen).unwrap_or(1);
    store
        .put_call(role, primary, &call.call_ref, body, &[], 60_000, gen, &PutOpts::default())
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// (1) takeover tagging + handback targeting.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn deactivate_targets_filter_by_primary_and_as_of() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w1", repl);

    // Two takeover copies for different primaries, activated at t=100.
    let c0 = build_initial_call(&invite("w0", "w1", "cid-a"), src(), &config_for("w0"), 0);
    let c2 = build_initial_call(&invite("w2", "w1", "cid-b"), src(), &config_for("w2"), 0);
    let (r0, r2) = (c0.call_ref.clone(), c2.call_ref.clone());
    assert!(r0.starts_with("w0|") && r2.starts_with("w2|"));
    state.create(c0);
    state.create(c2);
    state.mark_takeover(&r0, 100);
    state.mark_takeover(&r2, 100);

    // Scoped to the asking primary, bounded by as_of (activated <= as_of).
    assert_eq!(state.deactivate_targets("w0", 150), vec![r0.clone()]);
    assert_eq!(state.deactivate_targets("w2", 150), vec![r2.clone()]);
    // Activated at 100 > as_of 50 → a later episode, left serving.
    assert!(state.deactivate_targets("w0", 50).is_empty());
    // A primary we hold no takeover for.
    assert!(state.deactivate_targets("w9", i64::MAX).is_empty());
}

// ---------------------------------------------------------------------------
// (2) local-only handback: shed the live copy, keep the backup Element.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn drop_local_sheds_live_copy_but_keeps_backup_element() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w1", repl.clone());

    // A call w0 owns + we (w1) back up: seed bak:w0, then hydrate the takeover.
    let call = build_initial_call(&invite("w0", "w1", "cid-h"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    put(&repl, BAK, "w0", &call).await;

    let (_c, fresh) = state.hydrate_from_replica(&r).await.expect("hydrate from bak:w0");
    assert!(fresh, "first hydrate materialises a fresh takeover copy");
    state.mark_takeover(&r, 100);
    assert!(state.peek(&r).is_some(), "takeover copy is live");

    // Handback (local-only).
    assert!(state.drop_local(&r), "dropped a live copy");
    assert!(state.peek(&r).is_none(), "live copy gone from the map");
    assert!(state.deactivate_targets("w0", i64::MAX).is_empty(), "takeover tag cleared");
    // The crux: NO delete propagated — the backup Element survives so the call
    // lives on at its reclaiming primary.
    assert!(
        repl.get_call(BAK, "w0", &r).await.unwrap().is_some(),
        "bak:w0 Element untouched by the local-only handback"
    );
    // Dropping a ref we no longer hold is a harmless no-op.
    assert!(!state.drop_local(&r), "second drop reports nothing dropped");
}

// ---------------------------------------------------------------------------
// (3) active reclaim read-paths + idempotent materialisation.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn reclaim_scan_materialises_pri_partition_idempotently() {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let state = call_state("w0", repl.clone());

    // A call w0 reclaimed into its own pri:w0 partition (via bootstrap).
    let call = build_initial_call(&invite("w0", "w1", "cid-r"), src(), &config_for("w0"), 0);
    let r = call.call_ref.clone();
    put(&repl, PRI, "w0", &call).await;

    // Bulk sweep sees it.
    let scanned = state.reclaim_scan().await;
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].call_ref, r);

    // Materialise into the live map: first inserts, second is a no-op.
    assert!(state.materialize_if_absent(scanned[0].clone()), "first materialise inserts");
    assert!(state.peek(&r).is_some(), "now live + routable");
    assert!(!state.materialize_if_absent(scanned[0].clone()), "second materialise is a no-op");

    // Reactive read-path returns the same call; a backup-role ref never reclaims.
    assert_eq!(state.peek_reclaimable(&r).await.map(|c| c.call_ref), Some(r.clone()));
    assert!(
        state.peek_reclaimable("w5|other|t").await.is_none(),
        "a ref whose primary isn't us is not reclaimable here"
    );
}
