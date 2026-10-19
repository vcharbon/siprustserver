//! The decode of a replicated call body bounds the nesting it follows: a
//! body is a peer's bytes, and the opaque per-service `ext` values are JSON
//! of any depth, so a body nested past the bound is refused before its
//! recursion can exhaust the stack.

use crate::common::representative_call;
use call::{CallBodyCodec, CallDecodeError, MsgpackCodec};

/// A JSON value `depth` arrays deep.
fn nested(depth: usize) -> serde_json::Value {
    (0..depth).fold(serde_json::Value::Null, |inner, _| serde_json::Value::Array(vec![inner]))
}

/// The representative call with one leg's `ext` slice holding `value`.
fn call_holding(value: serde_json::Value) -> call::Call {
    let mut call = representative_call();
    call.a_leg.ext = Some([("svc".to_string(), value)].into_iter().collect());
    call
}

#[test]
fn a_body_nested_past_the_bound_is_refused() {
    let codec = MsgpackCodec::new();
    let body = codec.encode(&call_holding(nested(200))).expect("encodes");
    match codec.decode(&body) {
        Err(CallDecodeError::Decode(_)) => {}
        other => panic!("a body nested 200 deep decodes: {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_body_nested_within_the_bound_decodes() {
    let codec = MsgpackCodec::new();
    let call = call_holding(nested(16));
    let body = codec.encode(&call).expect("encodes");
    assert_eq!(codec.decode(&body).expect("decodes"), call);
}
