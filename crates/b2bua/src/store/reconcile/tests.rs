use std::net::SocketAddr;
use std::sync::Arc;

use call::Call;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::*;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::store::{CallStore, InMemoryCallStore, LOCK_RECOVERIES};

fn call(call_ref: &str, call_id: &str) -> Call {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bK-reconcile\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@192.0.2.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("expected a request")
    };
    let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let src = SocketAddr::from(([192, 0, 2, 9], 5060));
    let mut call = build_initial_call(&req, src, &cfg, &sip_txn::IdGen::seeded(1), 0);
    call.call_ref = call_ref.into();
    call
}

fn state() -> CallState {
    let store: Arc<dyn CallStore> = Arc::new(InMemoryCallStore::new());
    CallState::new(store, "w0", B2buaMetrics::new())
}

#[test]
fn a_whole_store_needs_no_correction() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    state.create(call("b", "b@192.0.2.9"));
    assert!(state.reconcile(7).is_clean());
    assert_eq!(state.touched_count(), 2);
}

/// An insert a panic cut after the call landed but before its stamp: the
/// next pass stamps it at the pass's instant.
#[test]
fn a_call_with_no_stamp_is_stamped_by_the_next_pass() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    state.locked().touched.remove("a");
    assert_eq!(state.last_touched("a"), None);

    let fixed = state.reconcile(42);
    assert_eq!(fixed, Reconciled { missing_stamps: 1, ..Reconciled::default() });
    assert_eq!(state.last_touched("a"), Some(42));
    assert!(state.reconcile(43).is_clean(), "the store is whole again");
}

/// A removal clears the call's takeover mark with it.
#[test]
fn a_removed_call_leaves_no_takeover_mark() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    state.mark_takeover("a");
    state.remove("a");
    assert!(!state.is_takeover("a"));
    assert!(state.reconcile(5).is_clean());
}

/// A takeover mark whose call a cut removal left gone is dropped by the next
/// pass and counted, the store's other entries untouched.
#[test]
fn a_takeover_mark_of_a_gone_call_is_dropped_by_the_next_pass() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    state.create(call("b", "b@192.0.2.9"));
    state.mark_takeover("a");
    state.mark_takeover("b");
    {
        let mut inner = state.locked();
        inner.calls.remove("a");
        inner.touched.remove("a");
        let keys = inner.indexed.remove("a").unwrap_or_default();
        inner.unindex("a", &keys);
    }

    let fixed = state.reconcile(6);
    assert_eq!(fixed, Reconciled { orphan_takeover: 1, ..Reconciled::default() });
    assert!(!state.is_takeover("a"), "the orphan mark is dropped");
    assert!(state.is_takeover("b"), "a resident call's mark stays");
    assert!(state.reconcile(7).is_clean(), "the store is whole again");
}

/// A removal a panic cut after the call left: its stamp and its index entries
/// are dropped, and a key another live call holds is left where it is.
#[test]
fn the_entries_of_a_call_that_is_gone_are_dropped() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    state.create(call("b", "b@192.0.2.9"));
    let held_by_b: Vec<String> = state.locked().indexed.get("b").cloned().unwrap();
    {
        let mut inner = state.locked();
        inner.calls.remove("a");
        // A key of `b` that `a`'s stale list also names stays with `b`.
        inner.indexed.get_mut("a").unwrap().push(held_by_b[0].clone());
    }

    let fixed = state.reconcile(9);
    assert_eq!(fixed.orphan_stamps, 1);
    assert!(fixed.orphan_index >= 2, "a's key list and a's keys: {fixed:?}");
    let inner = state.locked();
    assert!(!inner.touched.contains_key("a") && !inner.indexed.contains_key("a"));
    assert!(inner.sip_index.values().all(|owner| owner == "b"), "no key names a");
    for key in &held_by_b {
        assert_eq!(inner.sip_index.get(key).map(String::as_str), Some("b"), "{key} stays b's");
    }
}

/// A resident call that lost its key list is indexed again, without taking
/// a key another resident call holds.
#[test]
fn a_call_with_no_key_list_is_indexed_again() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    let keys = {
        let mut inner = state.locked();
        let keys = inner.indexed.remove("a").unwrap();
        for k in &keys {
            inner.sip_index.remove(k);
        }
        keys
    };
    let fixed = state.reconcile(5);
    assert_eq!(fixed, Reconciled { missing_index: 1, ..Reconciled::default() });
    let inner = state.locked();
    for key in &keys {
        assert_eq!(inner.sip_index.get(key).map(String::as_str), Some("a"));
    }
}

/// A panic under the call map's lock leaves the store usable: the lock is
/// taken as it stands and the recovery is counted under its name.
#[test]
fn a_poisoned_call_map_lock_is_recovered_and_counted() {
    let state = state();
    state.create(call("a", "a@192.0.2.9"));
    let before = LOCK_RECOVERIES.count(super::super::poison::CALL_STORE);
    let poisoner = state.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.locked();
        panic!("expected: a panic under the call map's lock");
    })
    .join();

    assert!(state.contains("a"), "the store answers after the panic");
    state.create(call("b", "b@192.0.2.9"));
    assert_eq!(state.active_count(), 2);
    assert_eq!(LOCK_RECOVERIES.count(super::super::poison::CALL_STORE) - before, 1);
}
