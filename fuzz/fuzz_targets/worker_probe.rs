//! The health probe reading a worker's OPTIONS answer (ADR-0050 §3): on UDP
//! anything can answer from a worker's address. Worker a answers the probe's
//! first round with the input's status and header lines (its `X-Overload`
//! load report, `X-Readiness`, `X-Incarnation`), then the caller's INVITE is
//! routed on what the probe read. The probe's loop and the recv loop keep
//! turning.

#![no_main]

use fuzz_lanes::input::probe_case;
use fuzz_lanes::on_lane_stack;
use fuzz_lanes::proxy_lane::probe;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let (status, lines) = probe_case(&data);
        let status = probe::STATUSES[usize::from(status) % probe::STATUSES.len()];
        let probed = probe::case(status, lines);
        assert_eq!(probed.outcome.failure, None);
    });
});
