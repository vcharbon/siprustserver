//! `callRef` derive/parse tests.

use call::{derive_call_ref, parse_call_ref};

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
    // Two-segment ref (no ordinal) → None so callers can upgrade it.
    assert!(parse_call_ref("cid|tag").is_none());
    // Malformed shapes.
    assert!(parse_call_ref("").is_none());
    assert!(parse_call_ref("nopipe").is_none());
    assert!(parse_call_ref("|cid|tag").is_none()); // empty primary
    assert!(parse_call_ref("p||tag").is_none()); // empty callId
    assert!(parse_call_ref("p|cid|").is_none()); // empty fromTag
}
