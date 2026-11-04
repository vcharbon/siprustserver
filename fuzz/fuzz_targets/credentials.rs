//! Digest credentials and challenges a peer sends (RFC 3261 §22, RFC 2617):
//! each line of the input as each header of the authentication family in a
//! request, read through the parser as a peer's line is. Every value read
//! renders to text that, in the same request, reads back to the same
//! rendering, and the request refreezes.

#![no_main]

use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use sip_message::header::{
    Authorization, HeaderValue, ProxyAuthenticate, ProxyAuthorization, WwwAuthenticate,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        for line in String::from_utf8_lossy(&data).lines() {
            round_trip::<Authorization>(line);
            round_trip::<ProxyAuthorization>(line);
            round_trip::<WwwAuthenticate>(line);
            round_trip::<ProxyAuthenticate>(line);
        }
    });
});

/// A REGISTER carrying `value` as header `H`, as the parser reads it.
fn request<H: HeaderValue>(value: &str) -> Result<SipMessage, sip_message::SipParseError> {
    let raw = format!(
        "REGISTER sip:registrar.example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-fuzz\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:alice@example.com>\r\n\
         Call-ID: fuzz@192.0.2.1\r\n\
         CSeq: 1 REGISTER\r\n\
         {}: {value}\r\n\
         Content-Length: 0\r\n\r\n",
        H::header_name()
    );
    CustomParser::new().parse(raw.as_bytes())
}

fn round_trip<H: HeaderValue>(line: &str) {
    let Ok(Some(Ok(value))) = request::<H>(line).map(|m| m.header::<H>()) else { return };
    let name = H::header_name();
    let wire = value.to_wire();
    let again = request::<H>(&wire).unwrap_or_else(|e| {
        panic!("{name} {line:?} renders as {wire:?}, which its request refuses: {}", e.reason)
    });
    match again.header::<H>() {
        Some(Ok(v)) => assert_eq!(v.to_wire(), wire, "{name} {line:?} renders to itself"),
        other => panic!("{name} {line:?} renders as {wire:?}, which reads as {other:?}"),
    }
    if let SipMessage::Request(r) = &again {
        if let Err(e) = r.thaw().freeze() {
            panic!("{name} {line:?} renders as {wire:?}, whose request does not refreeze: {e}");
        }
    }
}
