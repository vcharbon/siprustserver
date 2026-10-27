//! SDP offer/answer over two descriptions a peer controls (ADR-0051 §5: the
//! answer is built inside a call's turn): every read, answer and
//! restatement of the message lane's SDP probe, each description in turn
//! the one offered.

#![no_main]

use fuzz_lanes::input::pair;
use fuzz_lanes::message_lane::probe::{description, Digest};
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let (a, b) = pair(&data);
        let mut digest = Digest::new();
        description(a, b, &mut digest);
        description(b, a, &mut digest);
    });
});
