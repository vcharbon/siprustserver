//! The readers of the message layer never stall (ADR-0057 §5): each loop over
//! the input moves at least one byte on per step, and a fuzz build panics on
//! a step that does not, where a release build refuses it. The input runs
//! whole through every entry point of the message lane's probe; each of its
//! lines as a header line of a request through the probe again, so a header's
//! readers meet any value; and each line through the readers that take text
//! alone: list splits, escapes, reason keys, MSCML and multipart framing.

#![no_main]

use fuzz_lanes::message_lane::probe::{message, Digest};
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use sip_message::char_class::{is_paramchar, run_refusal};
use sip_message::error::reason_key;
use sip_message::header::ItemSeparator;
use sip_message::param_codec::decode_param;
use sip_message::{decompose_multipart, mscml};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        message(&data, &mut Digest::new());
        for line in String::from_utf8_lossy(&data).lines() {
            message(&in_request(line), &mut Digest::new());
            text_readers(line);
        }
    });
});

/// A REGISTER carrying `line` as one of its header lines.
fn in_request(line: &str) -> Vec<u8> {
    format!(
        "REGISTER sip:registrar.example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-fuzz\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:alice@example.com>\r\n\
         Call-ID: fuzz@192.0.2.1\r\n\
         CSeq: 1 REGISTER\r\n\
         {line}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn text_readers(line: &str) {
    for sep in [ItemSeparator::Comma, ItemSeparator::PrivValue] {
        sep.split(line).for_each(drop);
    }
    run_refusal(line, is_paramchar, true);
    decode_param(line);
    reason_key(line);
    mscml::request(line);
    mscml::response(line);
    let (boundary, body) = line.split_once(' ').unwrap_or((line, ""));
    decompose_multipart(body.as_bytes(), boundary);
}
