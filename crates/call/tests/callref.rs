//! `callRef` derive/parse + index-key derivation tests.

mod common;

use call::{call_index_keys, call_index_keys_from_unknown, derive_call_ref, parse_call_ref};
use common::representative_call;

#[test]
fn derive_then_parse_round_trips() {
    let r = derive_call_ref("worker-0", "cid@host", "tag-abc");
    assert_eq!(r, "worker-0|cid@host|tag-abc");
    let parsed = parse_call_ref(&r).expect("well-formed");
    assert_eq!(parsed.primary, "worker-0");
    assert_eq!(parsed.call_id, "cid@host");
    assert_eq!(parsed.from_tag, "tag-abc");
}

#[test]
fn parse_rejects_malformed_and_legacy() {
    // Legacy two-segment ref (pre-HA) → None so callers can upgrade it.
    assert!(parse_call_ref("cid|tag").is_none());
    // Malformed shapes.
    assert!(parse_call_ref("").is_none());
    assert!(parse_call_ref("nopipe").is_none());
    assert!(parse_call_ref("|cid|tag").is_none()); // empty primary
    assert!(parse_call_ref("p||tag").is_none()); // empty callId
    assert!(parse_call_ref("p|cid|").is_none()); // empty fromTag
}

#[test]
fn index_keys_cover_every_leg_dialog_and_context() {
    let call = representative_call();
    let keys = call_index_keys(&call);
    assert!(keys.contains(&"leg:call-id-deadbeef@example.com|alice-from-tag-001".to_string()));
    assert!(keys.contains(&"leg:b-leg-call-id-fedcba@b2bua|b2bua-from-tag-bleg-5544".to_string()));
    assert!(keys.contains(&"leg:b-leg-call-id-fedcba@b2bua".to_string()));
    assert!(keys.contains(&"leg:b-leg-call-id-fedcba@b2bua|bob-to-tag-007".to_string()));
    assert!(keys.contains(&"ctx:ctx-abc-123".to_string()));
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
