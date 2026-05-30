//! Smoke tests for the pure header accessors / readers ported from
//! `MessageHelpers.ts`. The TS corpus has no dedicated MessageHelpers unit
//! test (only `MessageHelpers-random.test.ts`, which exercises the seeded-RNG
//! identifier generators — deferred to slice 2); these lock the behaviour the
//! generators port depends on.

use sip_message::message_helpers::{
    extract_contact_uri, extract_name_addr_uri, extract_tag, get_header, get_headers, parse_sip_uri,
    parse_via_params, remove_header, set_header, strip_tag,
};
use sip_message::SipHeader;

fn hdr(name: &str, value: &str) -> SipHeader {
    SipHeader { name: name.to_string(), value: value.to_string() }
}

fn headers() -> Vec<SipHeader> {
    vec![
        hdr("Via", "SIP/2.0/UDP a;branch=z1"),
        hdr("Via", "SIP/2.0/UDP b;branch=z2"),
        hdr("From", "\"Alice\" <sip:alice@example.com>;tag=abc;foo=bar"),
        hdr("To", "<sip:bob@example.com>"),
        hdr("Contact", "<sip:bob@192.0.2.1:5070>"),
    ]
}

#[test]
fn get_header_is_case_insensitive_first_match() {
    assert_eq!(get_header(&headers(), "via"), Some("SIP/2.0/UDP a;branch=z1"));
    assert_eq!(get_header(&headers(), "CALL-ID"), None);
}

#[test]
fn get_headers_returns_all_in_order() {
    assert_eq!(
        get_headers(&headers(), "Via"),
        vec!["SIP/2.0/UDP a;branch=z1", "SIP/2.0/UDP b;branch=z2"]
    );
}

#[test]
fn set_header_replaces_first_or_appends() {
    let updated = set_header(&headers(), "To", "<sip:carol@example.com>");
    assert_eq!(get_header(&updated, "To"), Some("<sip:carol@example.com>"));
    let added = set_header(&headers(), "Max-Forwards", "70");
    assert_eq!(get_header(&added, "Max-Forwards"), Some("70"));
}

#[test]
fn remove_header_drops_all_matches() {
    let updated = remove_header(&headers(), "via");
    assert!(get_headers(&updated, "Via").is_empty());
}

#[test]
fn extract_and_strip_tag_are_quote_aware() {
    let from = "\"Alice\" <sip:alice@example.com>;tag=abc;foo=bar";
    assert_eq!(extract_tag(from), Some("abc".to_string()));
    // strip_tag rebuilds display-name + uri + remaining params, dropping tag.
    assert_eq!(strip_tag(from), "\"Alice\" <sip:alice@example.com>;foo=bar");
    // No tag → returned unchanged.
    assert_eq!(strip_tag("<sip:bob@example.com>"), "<sip:bob@example.com>");
}

#[test]
fn extract_uris() {
    assert_eq!(extract_name_addr_uri("\"Alice\" <sip:alice@example.com>;tag=x"), "sip:alice@example.com");
    assert_eq!(extract_contact_uri("<sip:bob@192.0.2.1:5070>"), "sip:bob@192.0.2.1:5070");
}

#[test]
fn parse_sip_uri_defaults_port_5060() {
    let parsed = parse_sip_uri("<sip:bob@example.com>").unwrap();
    assert_eq!(parsed.host, "example.com");
    assert_eq!(parsed.port, 5060);
    let with_port = parse_sip_uri("sip:bob@192.0.2.1:5070").unwrap();
    assert_eq!(with_port.port, 5070);
}

#[test]
fn parse_via_params_extracts_branch_cr_lg() {
    let p = parse_via_params("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1;cr=cref1;lg=a");
    assert_eq!(p.branch.as_deref(), Some("z9hG4bK1"));
    assert_eq!(p.cr.as_deref(), Some("cref1"));
    assert_eq!(p.lg.as_deref(), Some("a"));
}
