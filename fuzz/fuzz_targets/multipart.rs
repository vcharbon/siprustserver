//! A multipart body a peer sends (RFC 2046 §5.1): its parts read under the
//! input's boundary, read off its Content-Type, and composed again.

#![no_main]

use fuzz_lanes::input::multipart_case;
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use sip_message::{compose_multipart, decompose_multipart, entity_parts};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let (boundary, body) = multipart_case(&data);
        let boundary = String::from_utf8_lossy(boundary);
        let _ = decompose_multipart(body, &boundary);
        let content_type = format!("multipart/mixed;boundary={boundary}");
        let parts = entity_parts(Some(&content_type), &[], body);
        let _ = compose_multipart(&content_type, &parts);
    });
});
