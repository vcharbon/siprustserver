//! SIP routing index tests: a call's keys and the probe order of a lookup.

use crate::common::representative_call;
use call::{call_index_keys, call_index_keys_from_unknown, IndexLookup, KeyKind, Probe};

#[test]
fn index_keys_cover_every_leg_dialog_and_context() {
    let call = representative_call();
    let keys = call_index_keys(&call);
    assert_eq!(
        keys,
        vec![
            "a:call-id-deadbeef@example.com|alice-from-tag-001".to_string(),
            "bl:b-leg-call-id-fedcba@b2bua|b2bua-from-tag-bleg-5544".to_string(),
            "b:b-leg-call-id-fedcba@b2bua".to_string(),
            "b:b-leg-call-id-fedcba@b2bua|bob-to-tag-007".to_string(),
            "ctx:ctx-abc-123".to_string(),
        ]
    );
}

/// The keys a lookup probes, in order, as it writes them.
fn probed(lookup: IndexLookup<'_>) -> Vec<(bool, String)> {
    let mut buf = lookup.key_buffer();
    let capacity = buf.capacity();
    lookup
        .probes()
        .iter()
        .map(|probe| {
            let (refuse, kind) = match *probe {
                Probe::Resolve(kind) => (false, kind),
                Probe::Refuse(kind) => (true, kind),
            };
            lookup.write_key(kind, &mut buf);
            assert_eq!(buf.capacity(), capacity, "the key buffer never grows");
            (refuse, buf.clone())
        })
        .collect()
}

/// A CANCEL probes the remote parties' tags only; a peer tag also stops at an
/// outgoing leg's own tag before falling back to its Call-ID.
#[test]
fn lookups_probe_in_order() {
    assert_eq!(
        probed(IndexLookup::Cancel { call_id: "x", from_tag: "f" }),
        vec![(false, "a:x|f".into()), (false, "b:x|f".into())]
    );
    assert_eq!(
        probed(IndexLookup::Peer { call_id: "x", tag: "t" }),
        vec![
            (false, "a:x|t".into()),
            (false, "b:x|t".into()),
            (true, "bl:x|t".into()),
            (false, "b:x".into()),
        ]
    );
}

/// A key's namespace names the leg that owns it: the incoming leg, or the
/// outgoing leg on the Call-ID (holding the dialog, for a dialog key).
#[test]
fn a_matched_key_names_the_leg_that_owns_it() {
    let call = representative_call();
    let a =
        IndexLookup::Peer { call_id: "call-id-deadbeef@example.com", tag: "alice-from-tag-001" };
    let b_call_id = "b-leg-call-id-fedcba@b2bua";
    let bob = IndexLookup::Peer { call_id: b_call_id, tag: "bob-to-tag-007" };
    let untagged = IndexLookup::Peer { call_id: b_call_id, tag: "" };
    let own = IndexLookup::Peer { call_id: b_call_id, tag: "b2bua-from-tag-bleg-5544" };
    assert_eq!(a.owning_leg(KeyKind::ALeg, &call), Some("a"));
    assert_eq!(bob.owning_leg(KeyKind::BDialog, &call), Some("b-1"));
    assert_eq!(untagged.owning_leg(KeyKind::BLeg, &call), Some("b-1"));
    assert_eq!(own.owning_leg(KeyKind::BLocal, &call), None, "an own tag resolves no leg");
    assert_eq!(bob.owning_leg(KeyKind::ALeg, &call), None, "not the incoming identity");
    let stranger = IndexLookup::Peer { call_id: b_call_id, tag: "carol" };
    assert_eq!(stranger.owning_leg(KeyKind::BDialog, &call), None, "no dialog with that tag");
}

/// Parity property the source relies on: a well-shaped value yields the same
/// keys through the schema-tolerant walk as through the typed extractor.
#[test]
fn from_unknown_matches_typed_extractor() {
    let call = representative_call();
    let value = serde_json::to_value(&call).expect("to_value");
    assert_eq!(call_index_keys_from_unknown(&value), call_index_keys(&call));
}

#[test]
fn from_unknown_tolerates_garbage() {
    assert!(call_index_keys_from_unknown(&serde_json::json!(null)).is_empty());
    assert!(call_index_keys_from_unknown(&serde_json::json!("a string")).is_empty());
    assert!(call_index_keys_from_unknown(&serde_json::json!({ "b_legs": 7 })).is_empty());
}
