//! A worker's load report as the proxy reads it: each line an `X-Overload`
//! value fed to the load observer a probe interval after the last, and an
//! `X-Readiness` value. A payload that reads is in range, and the observer's
//! AIMD step, bucket and stale sweep take any sequence of them.

#![no_main]

use fuzz_lanes::proxy_lane::report;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    report::case(data);
});
