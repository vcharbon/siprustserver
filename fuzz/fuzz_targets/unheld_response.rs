//! A response for a call this node does not hold (ADR-0055 §4, §12): the
//! ACK, BYE and rejecting answer built from a final's own Contact,
//! Record-Route and SDP when its via-extension verifies. The seeds carry a
//! validly signed extension, so mutations of the rest of the final reach the
//! builders; a mutation of the extension exercises the verifier. A verified
//! final is answered as it is owed.

#![no_main]

use fuzz_lanes::on_lane_stack;
use fuzz_lanes::unheld_lane::case;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        case::case(&data);
    });
});
