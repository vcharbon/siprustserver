//! Optional structured headers — the eager, non-fatal accessor model. Port of
//! `tests/sip/lazy-headers.test.ts`.
//!
//! In the TS source these were lazily parsed on first `getHeader(name)`; ADR-0003
//! ports them as **eager + non-fatal** fields on [`OptionalHeaders`] (parsed once
//! at parse time, a malformed value captured as `Err` rather than rejecting the
//! message). So `msg.getHeader("p-asserted-identity")` → `msg.optional().p_asserted_identity`.
//! The TS "memoization — second call returns the same Result" tests become the
//! trivially-true "the stored field is the same reference on each access".

use sip_message::{CustomParser, NameAddr, ParamValue, Rack, SipMessage, SipParser};

const BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: lazy-test\r\n\
CSeq: 1 INVITE\r\n";

const PRACK_BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: lazy-test\r\n\
CSeq: 2 PRACK\r\n";

const REFER_BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: lazy-test\r\n\
CSeq: 3 REFER\r\n";

fn build(start: &str, base: &str, optional: &str) -> SipMessage {
    let raw = format!("{start} sip:bob@example.com SIP/2.0\r\n{base}{optional}Content-Length: 0\r\n\r\n");
    CustomParser::new().parse(raw.as_bytes()).expect("parse should succeed")
}

fn invite(optional: &str) -> SipMessage {
    build("INVITE", BASE, optional)
}

fn na_param<'a>(na: &'a NameAddr, key: &str) -> Option<&'a str> {
    match na.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.as_str()),
        _ => None,
    }
}

// --- P-Asserted-Identity (RFC 3325) ---

#[test]
fn pai_absent_is_empty_list() {
    let msg = invite("");
    assert_eq!(msg.optional().p_asserted_identity.as_ref().unwrap(), &Vec::<NameAddr>::new());
}

#[test]
fn pai_single_value() {
    let msg = invite("P-Asserted-Identity: \"Cullen Jennings\" <sip:fluffy@cisco.com>\r\n");
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].display_name.as_deref(), Some("Cullen Jennings"));
    assert_eq!(list[0].uri, "sip:fluffy@cisco.com");
}

#[test]
fn pai_two_values_comma_separated() {
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>, <tel:+14085551212>\r\n");
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].uri, "sip:fluffy@cisco.com");
    assert_eq!(list[1].uri, "tel:+14085551212");
}

#[test]
fn pai_two_header_instances_combine() {
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>\r\nP-Asserted-Identity: <tel:+14085551212>\r\n");
    assert_eq!(msg.optional().p_asserted_identity.as_ref().unwrap().len(), 2);
}

#[test]
fn pai_field_is_stored_once_eagerly() {
    // The TS "memoization" test: in the eager model the parsed value is stored
    // on the message, so two accesses yield the same reference.
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>\r\n");
    let a = &msg.optional().p_asserted_identity;
    let b = &msg.optional().p_asserted_identity;
    assert!(std::ptr::eq(a, b));
}

#[test]
fn pai_comma_splitter_ignores_quoted_commas() {
    let msg = invite("P-Asserted-Identity: \"Smith, John\" <sip:js@example.com>, <sip:jane@example.com>\r\n");
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].display_name.as_deref(), Some("Smith, John"));
    assert_eq!(list[0].uri, "sip:js@example.com");
    assert_eq!(list[1].uri, "sip:jane@example.com");
}

// --- Diversion (RFC 5806) ---

#[test]
fn diversion_entry_params() {
    let msg = invite("Diversion: <sip:divert@example.com>;reason=user-busy;counter=2;privacy=full\r\n");
    let list = msg.optional().diversion.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uri, "sip:divert@example.com");
    assert_eq!(na_param(&list[0], "reason"), Some("user-busy"));
    assert_eq!(na_param(&list[0], "counter"), Some("2"));
    assert_eq!(na_param(&list[0], "privacy"), Some("full"));
}

// --- History-Info (RFC 7044) ---

#[test]
fn history_info_indexed_entries() {
    let msg = invite("History-Info: <sip:alice@example.com>;index=1\r\nHistory-Info: <sip:redirect@example.com>;index=1.1\r\n");
    let list = msg.optional().history_info.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(na_param(&list[0], "index"), Some("1"));
    assert_eq!(na_param(&list[1], "index"), Some("1.1"));
}

// --- Geolocation / -Routing / -Error (RFC 6442 / 7378) ---

#[test]
fn geolocation_list() {
    let msg = invite("Geolocation: <https://ls.example.com/loc1>, <https://ls.example.com/loc2>\r\n");
    let list = msg.optional().geolocation.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].uri, "https://ls.example.com/loc1");
    assert_eq!(list[1].uri, "https://ls.example.com/loc2");
}

#[test]
fn geolocation_routing_yes_no_absent_invalid() {
    assert_eq!(*invite("Geolocation-Routing: yes\r\n").optional().geolocation_routing.as_ref().unwrap(), Some(true));
    assert_eq!(*invite("Geolocation-Routing: NO\r\n").optional().geolocation_routing.as_ref().unwrap(), Some(false));
    assert_eq!(*invite("").optional().geolocation_routing.as_ref().unwrap(), None);
    assert!(invite("Geolocation-Routing: maybe\r\n").optional().geolocation_routing.is_err());
}

#[test]
fn geolocation_error_with_code_param() {
    let msg = invite("Geolocation-Error: <sip:locinfo@example.com>;code=200\r\n");
    let list = msg.optional().geolocation_error.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(na_param(&list[0], "code"), Some("200"));
}

// --- RAck (RFC 3262) ---

#[test]
fn rack_absent_is_none() {
    assert_eq!(*invite("").optional().rack.as_ref().unwrap(), None);
}

#[test]
fn rack_well_formed() {
    let msg = build("PRACK", PRACK_BASE, "RAck: 776656 1 INVITE\r\n");
    let rack = msg.optional().rack.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(rack, &Rack { rseq: 776656, seq: 1, method: "INVITE".to_string() });
}

#[test]
fn rack_extra_whitespace_tolerated() {
    let msg = build("PRACK", PRACK_BASE, "RAck:    42   7   INVITE\r\n");
    let rack = msg.optional().rack.as_ref().unwrap().clone().unwrap();
    assert_eq!(rack.rseq, 42);
    assert_eq!(rack.seq, 7);
    assert_eq!(rack.method, "INVITE");
}

#[test]
fn rack_missing_method_or_non_numeric_is_err() {
    assert!(build("PRACK", PRACK_BASE, "RAck: 1 2\r\n").optional().rack.is_err());
    assert!(build("PRACK", PRACK_BASE, "RAck: foo 2 INVITE\r\n").optional().rack.is_err());
}

// --- Refer-To (RFC 3515 + RFC 3891) ---

fn refer(optional: &str) -> SipMessage {
    build("REFER", REFER_BASE, optional)
}

#[test]
fn refer_to_absent_is_none() {
    assert_eq!(*invite("").optional().refer_to.as_ref().unwrap(), None);
}

#[test]
fn refer_to_blind_transfer() {
    let msg = refer("Refer-To: <sip:carol@example.com>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.uri, "sip:carol@example.com");
    assert!(r.replaces.is_none());
    assert!(r.embedded_headers.is_empty());
    assert_eq!(r.parsed_uri.as_ref().unwrap().user.as_deref(), Some("carol"));
    assert_eq!(r.parsed_uri.as_ref().unwrap().host, "example.com");
}

#[test]
fn refer_to_attended_with_replaces() {
    let msg = refer("Refer-To: <sip:carol@example.com?Replaces=abc-call-id%3Bto-tag%3Dt1%3Bfrom-tag%3Dt2>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.parsed_uri.as_ref().unwrap().user.as_deref(), Some("carol"));
    let replaces = r.replaces.as_ref().unwrap();
    assert_eq!(replaces.call_id, "abc-call-id");
    assert_eq!(replaces.to_tag, "t1");
    assert_eq!(replaces.from_tag, "t2");
    assert!(!replaces.early_only);
}

#[test]
fn refer_to_early_only() {
    let msg = refer("Refer-To: <sip:carol@example.com?Replaces=cid%3Bto-tag%3Dx%3Bfrom-tag%3Dy%3Bearly-only>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert!(r.replaces.as_ref().unwrap().early_only);
}

#[test]
fn refer_to_display_name_literal_replaces_not_matched() {
    let msg = refer("Refer-To: \"Replaces?replaces=foo\" <sip:carol@example.com>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.display_name.as_deref(), Some("Replaces?replaces=foo"));
    assert!(r.replaces.is_none());
    assert!(r.embedded_headers.is_empty());
}

#[test]
fn refer_to_replaces_missing_tags_keeps_raw_but_no_struct() {
    let msg = refer("Refer-To: <sip:carol@example.com?Replaces=just-callid>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert!(r.replaces.is_none());
    assert_eq!(r.embedded_headers.get("Replaces").map(String::as_str), Some("just-callid"));
}

#[test]
fn refer_to_header_level_params() {
    let msg = refer("Refer-To: <sip:carol@example.com>;method=INVITE\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert!(matches!(r.params.get("method"), Some(ParamValue::Value(v)) if v == "INVITE"));
    assert!(r.replaces.is_none());
}

// --- Remote-Party-ID, P-Preferred-Identity ---

#[test]
fn remote_party_id_params() {
    let msg = invite("Remote-Party-ID: \"Alice\" <sip:alice@example.com>;party=calling;id-type=subscriber;privacy=off\r\n");
    let list = msg.optional().remote_party_id.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(na_param(&list[0], "party"), Some("calling"));
    assert_eq!(na_param(&list[0], "id-type"), Some("subscriber"));
    assert_eq!(na_param(&list[0], "privacy"), Some("off"));
}

#[test]
fn p_preferred_identity_present() {
    let msg = invite("P-Preferred-Identity: <sip:alice@example.com>\r\n");
    let list = msg.optional().p_preferred_identity.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uri, "sip:alice@example.com");
}
