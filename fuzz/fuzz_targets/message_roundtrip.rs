//! A datagram through every entry point of the message layer (the message
//! lane's probe), and the round trips that guard header injection: a message
//! the wire grammar reads renders to text that reads back to the same
//! rendering, and thawed and frozen as a relay rewrites it, it passes the
//! write check every freeze runs and refreezes to itself. A freeze refuses
//! only a mandatory header the message arrived without.

#![no_main]

use fuzz_lanes::message_lane::probe::{message, Digest};
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use sip_message::draft::IncompleteDraft;
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, HeaderName, SipMessage, SipParser};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        message(&data, &mut Digest::new());
        round_trip(&data);
    });
});

fn round_trip(data: &[u8]) {
    let parser = CustomParser::new();
    let Ok(msg) = parser.parse(data) else { return };
    let wire = serialize(&msg);
    let again = reads(&parser, &wire, "a rendered message");
    assert_eq!(serialize(&again), wire, "a rendered message renders to itself");
    let Some(frozen) = refreeze(&msg) else { return };
    let refrozen = refreeze(&reads(&parser, &frozen, "a refrozen message"));
    assert_eq!(refrozen, Some(frozen), "a refrozen message refreezes to itself");
}

fn reads(parser: &CustomParser, bytes: &[u8], what: &str) -> SipMessage {
    parser.parse(bytes).unwrap_or_else(|e| panic!("{what} reads back: {e:?}"))
}

/// `msg` thawed and frozen: the render and write check of a relay; `None`
/// when `msg` lacks a header its direction requires.
fn refreeze(msg: &SipMessage) -> Option<Vec<u8>> {
    let frozen = match msg {
        SipMessage::Request(r) => r.thaw().freeze().map(|m| m.image().to_vec()),
        SipMessage::Response(r) => r.thaw().freeze().map(|m| m.image().to_vec()),
    };
    match frozen {
        Ok(image) => Some(image),
        Err(IncompleteDraft::Missing(names)) if names.iter().all(|n| !carries(msg, n)) => None,
        Err(e) => panic!("a message the wire grammar reads refreezes: {e}"),
    }
}

fn carries(msg: &SipMessage, name: &HeaderName) -> bool {
    msg.headers().iter().any(|h| name.matches(h.name.as_str()))
}
