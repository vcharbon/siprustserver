//! A replicated call body (ADR-0051 §5: a peer's snapshot is decoded on the
//! replication path): the call decode over any bytes, and every call decoded
//! writes back to itself.

#![no_main]

use call::{CallBodyCodec, MsgpackCodec};
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let codec = MsgpackCodec::new();
        let Ok(call) = codec.decode(&data) else { return };
        if let Ok(body) = codec.encode(&call) {
            let back = codec.decode(&body);
            assert_eq!(back.as_ref().ok(), Some(&call), "a decoded call writes back");
        }
    });
});
