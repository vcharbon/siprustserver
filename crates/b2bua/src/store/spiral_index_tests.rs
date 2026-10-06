//! The SIP routing index under a spiral (RFC 3261 §16.3): call 1's outgoing
//! leg comes back through a proxy as call 2's incoming leg, so both carry
//! Call-ID `X` and From-tag `F1`. Each lookup that reads the index finds the
//! call the message belongs to, whichever call wrote its keys last, and
//! either call's removal leaves the other's keys in place. The replicated
//! index a backup reads on takeover answers the same.
//!
//! Every message below is one the spiral produces when the `callRef` this
//! B2BUA stamps (Request-URI param, Via param) is missing:
//!
//! | message                                              | lookup          | owner  |
//! |------------------------------------------------------|-----------------|--------|
//! | CANCEL of call 2's incoming INVITE                   | Cancel{X, F1}   | call 2 |
//! | CANCEL of a re-INVITE call 2 sends to call 1         | Cancel{X, T2}   | call 1 |
//! | request call 1's outgoing leg sends to call 2        | Peer{X, F1}     | call 2 |
//! | response to call 2's request toward its caller       | Peer{X, F1}     | call 2 |
//! | request call 2's incoming leg sends to call 1        | Peer{X, T2}     | call 1 |
//! | response on call 1's outgoing leg (To-tag T2)        | Peer{X, T2}     | call 1 |
//! | response on call 1's outgoing leg with no To-tag     | Peer{X, ""}     | call 1 |

use std::net::SocketAddr;
use std::sync::Arc;

use call::helpers::{make_empty_dialog, MakeDialogLegCtx};
use call::{call_index_keys, Call, CallBodyCodec, IndexHit, IndexLookup, KeyKind, MsgpackCodec};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::{
    BufferedTerminateWriter, CallState, CallStore, InMemoryCallStore, PartitionRole, PutOpts,
};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::repl::ReplicatingCallStore;

/// Call 1's outgoing Call-ID, which call 2's incoming leg carries.
const X: &str = "x-1@b2bua";
/// Call 1's outgoing From-tag, which call 2's incoming leg carries.
const F1: &str = "f1";
/// The To-tag call 2 answers its incoming leg with: call 1's outgoing dialog tag.
const T2: &str = "t2";

fn invite(call_id: &str, from_tag: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{from_tag}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag={from_tag}\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A call on `(call_id, from_tag)` with one outgoing leg `(b_call_id, b_tag)`
/// whose dialog's remote tag is `b_peer_tag` (empty: no dialog yet).
fn call_with_b_leg(
    call_id: &str,
    from_tag: &str,
    b_call_id: &str,
    b_tag: &str,
    b_peer_tag: &str,
) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let src = SocketAddr::from(([10, 0, 0, 9], 5060));
    let call =
        build_initial_call(&invite(call_id, from_tag), src, &config, &sip_txn::IdGen::seeded(1), 0);
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    b.call_id = b_call_id.into();
    b.from_tag = b_tag.into();
    if !b_peer_tag.is_empty() {
        b.dialogs = vec![make_empty_dialog(
            &MakeDialogLegCtx {
                call_id: b_call_id,
                local_uri: "sip:alice@example.com",
                remote_uri: "sip:bob@example.com",
                local_tag: b_tag,
                remote_tag: b_peer_tag,
            },
            1,
        )];
    }
    call::helpers::add_b_leg(call, b)
}

/// Call 1 (alice's), its outgoing leg on `(X, F1)` ringing on dialog `T2`.
fn call_1() -> Call {
    call_with_b_leg("c1@alice", "alice", X, F1, T2)
}

/// Call 2, the spiral of call 1's outgoing leg, ringing its own callee.
fn call_2() -> Call {
    call_with_b_leg(X, F1, "y-2@b2bua", "f3", "bob")
}

fn cancel<'a>(call_id: &'a str, from_tag: &'a str) -> IndexLookup<'a> {
    IndexLookup::Cancel { call_id, from_tag }
}

fn peer<'a>(call_id: &'a str, tag: &'a str) -> IndexLookup<'a> {
    IndexLookup::Peer { call_id, tag }
}

fn state() -> CallState {
    let store = Arc::new(InMemoryCallStore::new()) as Arc<dyn CallStore>;
    CallState::new(store, "w0", B2buaMetrics::new())
}

/// Both calls resident, with `last` written to the index after the other.
fn both_resident(last: u8) -> (CallState, String, String) {
    let s = state();
    let (one, two) = (call_1(), call_2());
    let (r1, r2) = (one.call_ref.clone(), two.call_ref.clone());
    s.create(one);
    s.create(two);
    let rewrite = if last == 1 { &r1 } else { &r2 };
    let again = s.peek(rewrite).expect("resident");
    s.update(again);
    (s, r1, r2)
}

#[test]
fn every_lookup_finds_its_own_call_whichever_call_reindexed_last() {
    for last in [1, 2] {
        let (s, r1, r2) = both_resident(last);
        let at = |l| s.resolve_from_sip_key_sync(l).map(|h| h.call_ref);
        assert_eq!(at(cancel(X, F1)), Some(r2.clone()), "CANCEL, call {last} last");
        assert_eq!(at(cancel(X, T2)), Some(r1.clone()), "re-INVITE CANCEL, call {last} last");
        assert_eq!(at(peer(X, F1)), Some(r2.clone()), "peer tag F1, call {last} last");
        assert_eq!(at(peer(X, T2)), Some(r1.clone()), "peer tag T2, call {last} last");
        assert_eq!(at(peer(X, "")), Some(r1.clone()), "no To-tag, call {last} last");
    }
}

/// Each hit names the side that owns the identity, and so the leg the
/// message came from: call 2's incoming leg for call 1's outgoing tag, call
/// 1's outgoing leg for the tag its peer chose.
#[tokio::test]
async fn every_hit_names_the_side_that_owns_the_identity() {
    let hit = |call: &Call, kind| Some(IndexHit { call_ref: call.call_ref.clone(), kind });
    let (one, two) = (call_1(), call_2());
    let (s, _, _) = both_resident(1);
    let (backup, _, _, _) = backup_with_replicas(1).await;
    for (lookup, expected, leg) in [
        (cancel(X, F1), hit(&two, KeyKind::ALeg), "a"),
        (cancel(X, T2), hit(&one, KeyKind::BDialog), "b-1"),
        (peer(X, F1), hit(&two, KeyKind::ALeg), "a"),
        (peer(X, T2), hit(&one, KeyKind::BDialog), "b-1"),
        (peer(X, ""), hit(&one, KeyKind::BLeg), "b-1"),
    ] {
        assert_eq!(s.resolve_from_sip_key_sync(lookup), expected, "{lookup:?}");
        assert_eq!(
            backup.resolve_from_replica_index(lookup).await.unwrap(),
            expected,
            "{lookup:?}"
        );
        let found = expected.expect("every lookup here resolves");
        let owner = if found.call_ref == one.call_ref { &one } else { &two };
        assert_eq!(lookup.owning_leg(found.kind, owner), Some(leg), "{lookup:?}");
    }
}

/// The CANCEL races the INVITE that creates call 2: before call 2 is indexed
/// the lookup misses (the router then derives call 2's `callRef`); it never
/// lands on call 1 through its outgoing leg.
#[test]
fn a_cancel_never_resolves_through_an_outgoing_leg() {
    let s = state();
    s.create(call_1());
    assert_eq!(s.resolve_from_sip_key_sync(cancel(X, F1)).map(|h| h.call_ref), None);
}

/// A hop that keeps the Call-ID but mints its own From-tag sends back an
/// INVITE that shares only the Call-ID with call 1's outgoing leg; its CANCEL
/// still never resolves to call 1.
#[test]
fn a_cancel_sharing_only_the_call_id_of_an_outgoing_leg_misses() {
    let s = state();
    s.create(call_1());
    assert_eq!(s.resolve_from_sip_key_sync(cancel(X, "other-tag")).map(|h| h.call_ref), None);
}

#[test]
fn removing_one_call_leaves_the_others_keys() {
    for last in [1, 2] {
        let (s, r1, r2) = both_resident(last);
        s.remove(&r1);
        assert_eq!(
            s.resolve_from_sip_key_sync(cancel(X, F1)).map(|h| h.call_ref),
            Some(r2.clone())
        );
        assert_eq!(s.resolve_from_sip_key_sync(peer(X, F1)).map(|h| h.call_ref), Some(r2.clone()));

        let (s, r1, r2) = both_resident(last);
        s.remove(&r2);
        assert_eq!(s.resolve_from_sip_key_sync(peer(X, T2)).map(|h| h.call_ref), Some(r1.clone()));
        assert_eq!(s.resolve_from_sip_key_sync(peer(X, "")).map(|h| h.call_ref), Some(r1.clone()));
    }
}

/// With call 2 gone, a message for call 2's incoming leg (peer tag F1, call
/// 1's own outgoing tag) resolves to nothing — not to call 1 by Call-ID.
#[test]
fn a_message_naming_an_outgoing_legs_own_tag_is_never_its_calls() {
    let (s, _r1, r2) = both_resident(1);
    s.remove(&r2);
    assert_eq!(s.resolve_from_sip_key_sync(peer(X, F1)).map(|h| h.call_ref), None);
    assert_eq!(s.resolve_from_sip_key_sync(cancel(X, F1)).map(|h| h.call_ref), None);
}

/// A backup holding both calls' replicas, `last` put after the other.
async fn backup_with_replicas(last: u8) -> (CallState, Arc<ReplicatingCallStore>, Call, Call) {
    let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    let store = Arc::new(InMemoryCallStore::new()) as Arc<dyn CallStore>;
    let writer = BufferedTerminateWriter::spawn(repl.clone() as Arc<dyn CallStore>, 64);
    let s = CallState::new(store, "w1", B2buaMetrics::new()).with_replication(repl.clone(), writer);
    let (one, two) = (call_1(), call_2());
    let order: [&Call; 3] = if last == 1 { [&one, &two, &one] } else { [&two, &one, &two] };
    for call in order {
        put_replica(&repl, call).await;
    }
    (s, repl, one, two)
}

async fn put_replica(repl: &ReplicatingCallStore, call: &Call) {
    let body = MsgpackCodec::new().encode(call);
    let idx = call_index_keys(call);
    repl.put_call(
        PartitionRole::Backup,
        "w0",
        &call.call_ref,
        body,
        &idx,
        60_000,
        1,
        0,
        &PutOpts::default(),
    )
    .await
    .unwrap();
}

async fn delete_replica(repl: &ReplicatingCallStore, call: &Call) {
    repl.delete_call(PartitionRole::Backup, "w0", &call.call_ref, &[], false, &PutOpts::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn the_replicated_index_answers_every_lookup_like_the_live_one() {
    for last in [1, 2] {
        let (s, repl, one, two) = backup_with_replicas(last).await;
        let s = &s;
        let at =
            |l| async move { s.resolve_from_replica_index(l).await.map(|h| h.map(|h| h.call_ref)) };
        assert_eq!(at(cancel(X, F1)).await.unwrap(), Some(two.call_ref.clone()), "{last} last");
        assert_eq!(at(peer(X, F1)).await.unwrap(), Some(two.call_ref.clone()), "{last} last");
        assert_eq!(at(peer(X, T2)).await.unwrap(), Some(one.call_ref.clone()), "{last} last");

        delete_replica(&repl, &one).await;
        assert_eq!(at(cancel(X, F1)).await.unwrap(), Some(two.call_ref.clone()));
        assert_eq!(at(peer(X, F1)).await.unwrap(), Some(two.call_ref.clone()));

        let (s, repl, one, two) = backup_with_replicas(last).await;
        let s = &s;
        let at =
            |l| async move { s.resolve_from_replica_index(l).await.map(|h| h.map(|h| h.call_ref)) };
        delete_replica(&repl, &two).await;
        assert_eq!(at(peer(X, T2)).await.unwrap(), Some(one.call_ref.clone()));
        assert_eq!(at(peer(X, F1)).await.unwrap(), None, "call 2 gone: never call 1's");
        assert_eq!(at(cancel(X, F1)).await.unwrap(), None, "call 2 gone: never call 1's");
    }
}

/// A key two calls both state (a callback context) belongs to the call that
/// wrote it last; the other call leaving does not remove it.
#[test]
fn a_shared_key_stays_with_its_last_writer_when_the_other_call_leaves() {
    let s = state();
    let (mut one, mut two) = (call_1(), call_2());
    one.callback_context = Some("shared".into());
    two.callback_context = Some("shared".into());
    let r1 = one.call_ref.clone();
    s.create(one);
    s.create(two);
    s.remove(&r1);
    assert!(
        s.inner.lock().unwrap().sip_index.contains_key(&call::index_key::callback_key("shared")),
        "call 2's entry survives call 1's removal"
    );
}
