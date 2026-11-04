//! A replication peer's stream (ADR-0051 §5: the decode runs on the
//! replication task): the stream reader and the frame decoder over any
//! bytes, and every frame decoded writes back to itself.

#![no_main]

use fuzz_lanes::frame_lane::case::case;
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use repl_net::{decode_frame, encode_frame};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let (frames, bare) = case(&data);
        for frame in frames.iter().chain(&bare) {
            if let Ok(payload) = encode_frame(frame) {
                let back = decode_frame(&payload);
                assert_eq!(back.as_ref().ok(), Some(frame), "a decoded frame writes back");
            }
        }
    });
});
