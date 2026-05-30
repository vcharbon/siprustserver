//! X-Api-Call JSON integrity through the parser. Port of
//! `tests/sip/parser-x-api-call-fold.test.ts`.
//!
//! Regression: an extension header whose value contains commas (a JSON
//! payload) must NOT be split on those commas — only structured list headers
//! fold. The B2BUA's k8s reroute payload nests an `on_failure` object; it must
//! round-trip byte-identical so `InitialInviteHandler` can `JSON.parse` it.
//! (The TS `jssip`/`native` oracle columns are dropped — see ADR-0001.)

use sip_message::message_helpers::get_headers;
use sip_message::{CustomParser, SipMessage, SipParser};

const API_CALL_REROUTE: &str = r#"{"action":"route","destination":{"host":"172.20.0.1","port":25081},"new_ruri":"sip:bob1@172.20.0.1:25081","on_failure":{"action":"failover","destination":{"host":"172.20.0.1","port":25081},"new_ruri":"sip:bob2@172.20.0.1:25081"}}"#;

const API_CALL_SIMPLE: &str = r#"{"action":"route","destination":{"host":"172.20.0.1","port":25081},"new_ruri":"sip:bob@172.20.0.1:25081"}"#;

fn build_invite(x_api_call: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob1@kindlab SIP/2.0\r\n\
Via: SIP/2.0/UDP 5.1.1.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@kindlab>;tag=fromtag\r\n\
To: <sip:bob1@kindlab>\r\n\
Call-ID: test-call-id@5.1.1.1\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Contact: <sip:alice@5.1.1.1:5060>\r\n\
X-Api-Call: {x_api_call}\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn x_api_call_values(msg: &SipMessage) -> Vec<&str> {
    get_headers(msg.headers(), "x-api-call")
}

#[test]
fn simple_x_api_call_round_trips() {
    let msg = CustomParser::new().parse(&build_invite(API_CALL_SIMPLE)).expect("parse");
    let vals = x_api_call_values(&msg);
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0], API_CALL_SIMPLE);
}

#[test]
fn nested_x_api_call_round_trips() {
    let msg = CustomParser::new().parse(&build_invite(API_CALL_REROUTE)).expect("parse");
    let vals = x_api_call_values(&msg);
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0], API_CALL_REROUTE);
}

#[test]
fn last_write_wins_extraction_keeps_payload_intact() {
    // Replicates InitialInviteHandler's extractSipHeaders: skip structural
    // headers, keep the rest. The X-Api-Call value must survive whole.
    let msg = CustomParser::new().parse(&build_invite(API_CALL_REROUTE)).expect("parse");
    let structural = [
        "from", "to", "via", "contact", "content-type", "call-id", "cseq", "max-forwards",
        "content-length",
    ];
    let value = msg
        .headers()
        .iter()
        .filter(|h| !structural.contains(&h.name.to_ascii_lowercase().as_str()))
        .find(|h| h.name == "X-Api-Call")
        .map(|h| h.value.as_str());
    assert_eq!(value, Some(API_CALL_REROUTE));
    assert!(value.unwrap().contains("\"on_failure\""));
}
