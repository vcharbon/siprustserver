//! The counted length of a call body ([`MsgpackCodec::encoded_len`]): the
//! length of the body `encode` builds, read without building it.

use crate::common::representative_call;
use call::{CallBodyCodec, MsgpackCodec};

#[test]
fn the_counted_length_is_the_encoded_body_s() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    let pad = serde_json::json!({ "pad": "x".repeat(300) });
    call::helpers::set_call_ext(&mut call, "svc", Some(pad));
    let bytes = codec.encode(&call).expect("the call encodes");
    assert_eq!(codec.encoded_len(&call), Some(bytes.len() as u64));
}
