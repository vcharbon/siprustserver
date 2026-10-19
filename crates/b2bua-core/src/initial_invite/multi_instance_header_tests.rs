//! Repeatable SIP headers (`History-Info`, `Diversion`, `P-Asserted-Identity`,
//! `Contact`) spread across separate lines must reach the decision engine
//! intact — collapsing to the last line silently drops every earlier hop, and
//! nothing downstream can restore a value lost above the adapter.

use super::build_request;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

fn parse(raw: &str) -> SipRequest {
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

#[test]
fn repeated_header_lines_and_contacts_survive_in_wire_order() {
    let invite = parse(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-multi\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: multi@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             History-Info: <sip:a@ex.com>;index=1\r\n\
             History-Info: <sip:b@ex.com>;index=1.1\r\n\
             History-Info: <sip:c@ex.com>;index=1.1.1\r\n\
             Diversion: <sip:d1@ex.com>;reason=user-busy\r\n\
             Diversion: <sip:d2@ex.com>;reason=no-answer\r\n\
             Content-Length: 0\r\n\r\n",
    );
    let req = build_request(&invite);

    // Contact is populated via the plural getter, so any number of instances
    // survives (the parser caps an INVITE at one per RFC 3261 §8.1.1.8, but the
    // schema does not collapse — it carries whatever arrived).
    assert_eq!(req.contact, vec!["<sip:alice@10.0.0.9:5060>".to_string()]);

    // Every hop of a multi-line header survives, in wire order.
    assert_eq!(
        req.sip_header_values("History-Info").collect::<Vec<_>>(),
        vec!["<sip:a@ex.com>;index=1", "<sip:b@ex.com>;index=1.1", "<sip:c@ex.com>;index=1.1.1",],
    );
    assert_eq!(req.sip_header_values("Diversion").count(), 2);

    // The single-value accessor returns the first instance for the common read.
    assert_eq!(req.sip_header("History-Info"), Some("<sip:a@ex.com>;index=1"));
    assert_eq!(req.sip_header("Absent"), None);
}

/// The decision reads the INVITE's body byte for byte: a binary body is
/// never re-encoded on its way to the decision engine.
#[test]
fn a_binary_body_reaches_the_decision_byte_for_byte() {
    let body: &[u8] = &[0x77, 0x83, 0xFF, 0x00, 0x0A];
    let head = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKbin\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=1\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: binary-body\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Type: application/vnd.example.data\r\n\
             Content-Length: {}\r\n\r\n",
        body.len()
    );
    let raw = [head.as_bytes(), body].concat();
    let invite = match CustomParser::new().parse(&raw).expect("fixture INVITE should parse") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    };
    assert_eq!(build_request(&invite).sip_body.as_deref(), Some(body));
}

/// RFC 3261 §7.3.1: a header name is case-insensitive and its lines are one
/// ordered set, so a chain whose hops two elements spelled differently reaches
/// the decision engine in wire order, each line under the spelling it wore.
#[test]
fn build_request_keeps_wire_order_across_spellings() {
    let invite = parse(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKx\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=1\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: multi-spelling\r\n\
             CSeq: 1 INVITE\r\n\
             History-Info: <sip:a@ex.com>;index=1\r\n\
             X-Trunk: t1\r\n\
             history-info: <sip:b@ex.com>;index=1.1\r\n\
             HISTORY-INFO: <sip:c@ex.com>;index=1.1.1\r\n\
             x-trunk: t2\r\n\
             Content-Length: 0\r\n\r\n",
    );
    let req = build_request(&invite);
    assert_eq!(
        req.sip_headers,
        vec![
            ("Max-Forwards".to_string(), "70".to_string()),
            ("History-Info".to_string(), "<sip:a@ex.com>;index=1".to_string()),
            ("X-Trunk".to_string(), "t1".to_string()),
            ("history-info".to_string(), "<sip:b@ex.com>;index=1.1".to_string()),
            ("HISTORY-INFO".to_string(), "<sip:c@ex.com>;index=1.1.1".to_string()),
            ("x-trunk".to_string(), "t2".to_string()),
        ],
    );
    assert_eq!(
        req.sip_header_values("History-Info").collect::<Vec<_>>(),
        vec!["<sip:a@ex.com>;index=1", "<sip:b@ex.com>;index=1.1", "<sip:c@ex.com>;index=1.1.1"],
    );
    assert_eq!(req.sip_header_values("x-TRUNK").collect::<Vec<_>>(), vec!["t1", "t2"]);
}
