//! The call body's nesting bound holds on both sides: a body is a peer's
//! bytes, and the opaque per-service `ext` values are JSON of any depth, so a
//! body nested past the bound is refused at decode before its recursion can
//! exhaust the stack, and refused at encode so no peer is sent a body it
//! would refuse.

use crate::common::representative_call;
use call::codec::{EXT_JSON_DEPTH, MAX_DEPTH};
use call::{CallBodyCodec, CallDecodeError, MsgpackCodec};
use serde::Deserialize;

/// A JSON value `depth` arrays deep.
fn nested(depth: usize) -> serde_json::Value {
    (0..depth).fold(serde_json::Value::Null, |inner, _| serde_json::Value::Array(vec![inner]))
}

/// The representative call with its first callee leg's `ext` slice holding
/// `value`: the deepest place a service's JSON sits.
fn call_holding(value: serde_json::Value) -> call::Call {
    let mut call = representative_call();
    let leg = call.b_legs.first_mut().expect("the representative call has a callee");
    leg.ext = Some([("svc".to_string(), value)].into_iter().collect());
    call
}

/// The body of `call` as the serializer writes it, unchecked.
fn raw(call: &call::Call) -> Vec<u8> {
    rmp_serde::to_vec(call).expect("serializes")
}

/// The fewest levels of nesting a decoder of `bytes` needs.
fn depth_of(bytes: &[u8]) -> usize {
    (1..)
        .find(|&d| {
            let mut de = rmp_serde::Deserializer::from_read_ref(bytes);
            de.set_max_depth(d);
            call::Call::deserialize(&mut de).is_ok()
        })
        .expect("some depth decodes")
}

#[test]
fn a_body_nested_past_the_bound_is_refused() {
    let body = raw(&call_holding(nested(200)));
    match MsgpackCodec::new().decode(&body) {
        Err(CallDecodeError::Decode(_)) => {}
        other => panic!("a body nested 200 deep decodes: {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_call_nested_past_the_bound_is_refused_at_encode() {
    let encoded = MsgpackCodec::new().encode(&call_holding(nested(200)));
    assert!(encoded.is_err(), "a call nested 200 deep encodes: no peer would decode it");
}

/// Encode refuses exactly the bodies decode refuses, either side of the bound.
#[test]
fn encode_and_decode_share_the_bound() {
    let codec = MsgpackCodec::new();
    for depth in MAX_DEPTH - 8..MAX_DEPTH + 2 {
        let call = call_holding(nested(depth));
        let decodes = codec.decode(&raw(&call)).is_ok();
        assert_eq!(codec.encode(&call).is_ok(), decodes, "service JSON {depth} deep");
    }
}

/// The call tree's own nesting plus the JSON allowance fits the bound: a
/// service's JSON [`EXT_JSON_DEPTH`] deep in the deepest slot round-trips.
#[test]
fn the_call_tree_and_its_json_allowance_fit_the_bound() {
    let structure = depth_of(&raw(&representative_call()));
    assert!(structure + EXT_JSON_DEPTH < MAX_DEPTH, "{structure} + {EXT_JSON_DEPTH}");
    let codec = MsgpackCodec::new();
    let call = call_holding(nested(EXT_JSON_DEPTH));
    let body = codec.encode(&call).expect("JSON at the allowance encodes");
    assert_eq!(codec.decode(&body).expect("decodes"), call);
}
