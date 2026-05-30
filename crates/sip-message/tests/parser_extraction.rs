//! Parser extraction — raw bytes → parse → structured data, exercised through
//! the downstream header helpers and the eager typed fields. Port of the
//! custom-parser sections of `tests/sip/parser-extraction.test.ts`.
//!
//! The cross-parser "custom vs JsSIP" equivalence block is NOT ported — there
//! is no JsSIP in the Rust stack (ADR-0001); the `rvoip` parity oracle takes
//! that role in the compliance matrix.

use sip_message::message_helpers::{
    extract_contact_uri, extract_tag, get_header, get_headers, parse_sip_uri, parse_uri_params,
    parse_via_params, strip_tag,
};
use sip_message::{
    Contact, ContactSet, CustomParser, ParamValue, SipMessage, SipParser, SipRequest,
};

fn parse(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("parse should succeed")
}

fn req(raw: &str) -> SipRequest {
    match parse(raw) {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected request"),
    }
}

fn first_contact(cs: &ContactSet) -> Option<&Contact> {
    match cs {
        ContactSet::Contacts(c) => c.first(),
        ContactSet::Wildcard => None,
    }
}

fn via_param<'a>(params: &'a sip_message::Params, key: &str) -> Option<&'a str> {
    match params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.as_str()),
        _ => None,
    }
}

const BASIC_INVITE: &str = "INVITE sip:bob@example.com;transport=udp SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport;cr=call-ref-1;lg=a\r\n\
Via: SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK-prev;received=10.0.0.1\r\n\
Max-Forwards: 70\r\n\
From: \"Alice Smith\" <sip:alice@example.com>;tag=from-tag-xyz\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: unique-call-id@10.0.0.1\r\n\
CSeq: 42 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060;callRef=call-ref-1;leg=a>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 0\r\n\r\n";

const RESPONSE_WITH_TAGS: &str = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport;received=10.0.0.1;cr=ref-1;lg=b-1\r\n\
From: \"Alice\" <sip:alice@example.com>;tag=from-tag-1\r\n\
To: <sip:bob@example.com>;tag=to-tag-2\r\n\
Call-ID: test-call-id@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Content-Length: 0\r\n\r\n";

const COMPACT_FORM: &str = "INVITE sip:bob@example.com SIP/2.0\r\n\
v: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-compact\r\n\
f: <sip:alice@example.com>;tag=compact-tag\r\n\
t: <sip:bob@example.com>\r\n\
i: compact-call-id\r\n\
CSeq: 1 INVITE\r\n\
m: <sip:alice@10.0.0.1:5060>\r\n\
l: 0\r\n\r\n";

const MULTI_VIA: &str = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP hop3.example.com;branch=z9hG4bK-hop3;cr=ref;lg=b-1\r\n\
Via: SIP/2.0/UDP hop2.example.com;branch=z9hG4bK-hop2\r\n\
Via: SIP/2.0/TCP hop1.example.com;branch=z9hG4bK-hop1\r\n\
From: <sip:alice@example.com>;tag=multi-via\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: multi-via-call@example.com\r\n\
CSeq: 100 INVITE\r\n\
Content-Length: 0\r\n\r\n";

// Folded headers — continuation lines begin with a single leading space. Built
// with concat! so the significant leading SP survives (a `\`-newline string
// continuation would strip it).
const FOLDED_HEADERS: &str = concat!(
    "INVITE sip:bob@example.com SIP/2.0\r\n",
    "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-fold\r\n",
    " ;cr=folded-ref;lg=a\r\n",
    "From: \"Long Display Name\"\r\n",
    " <sip:alice@example.com>\r\n",
    " ;tag=folded-from-tag\r\n",
    "To: <sip:bob@example.com>\r\n",
    "Call-ID: folded-call-id@example.com\r\n",
    "CSeq: 1 INVITE\r\n",
    "Contact:\r\n",
    " <sip:alice@10.0.0.1:5060;callRef=fold-ref;leg=a>\r\n",
    "Content-Length: 0\r\n\r\n",
);

// --- structured header extraction via helpers ---

#[test]
fn from_tag_extraction_and_stripping() {
    let m = parse(BASIC_INVITE);
    let from = get_header(m.headers(), "From").unwrap();
    assert_eq!(extract_tag(from), Some("from-tag-xyz".to_string()));
    let stripped = strip_tag(from);
    assert!(!stripped.contains("tag="));
    assert!(stripped.contains("sip:alice@example.com"));
}

#[test]
fn to_tag_extraction() {
    assert_eq!(extract_tag(get_header(parse(BASIC_INVITE).headers(), "To").unwrap()), None);
    assert_eq!(
        extract_tag(get_header(parse(RESPONSE_WITH_TAGS).headers(), "To").unwrap()),
        Some("to-tag-2".to_string())
    );
}

#[test]
fn via_branch_cr_lg_extraction() {
    let m = parse(BASIC_INVITE);
    let top = get_headers(m.headers(), "Via")[0];
    let p = parse_via_params(top);
    assert_eq!(p.branch.as_deref(), Some("z9hG4bK-abc123"));
    assert_eq!(p.cr.as_deref(), Some("call-ref-1"));
    assert_eq!(p.lg.as_deref(), Some("a"));
}

#[test]
fn multiple_via_order_preserved() {
    let m = parse(MULTI_VIA);
    let vias = get_headers(m.headers(), "Via");
    assert_eq!(vias.len(), 3);
    assert_eq!(parse_via_params(vias[0]).branch.as_deref(), Some("z9hG4bK-hop3"));
    assert_eq!(parse_via_params(vias[1]).branch.as_deref(), Some("z9hG4bK-hop2"));
    assert_eq!(parse_via_params(vias[2]).branch.as_deref(), Some("z9hG4bK-hop1"));
}

#[test]
fn contact_uri_and_params_extraction() {
    let m = parse(BASIC_INVITE);
    let contact = get_header(m.headers(), "Contact").unwrap();
    let uri = extract_contact_uri(contact);
    assert_eq!(uri, "sip:alice@10.0.0.1:5060;callRef=call-ref-1;leg=a");
    let parsed = parse_sip_uri(&uri).unwrap();
    assert_eq!(parsed.host, "10.0.0.1");
    assert_eq!(parsed.port, 5060);
    assert_eq!(parsed.user.as_deref(), Some("alice"));
    // URI parameter names are lowercased.
    assert_eq!(parsed.params.get("callref").map(String::as_str), Some("call-ref-1"));
    assert_eq!(parsed.params.get("leg").map(String::as_str), Some("a"));
}

#[test]
fn request_uri_parsing_and_params() {
    let r = req(BASIC_INVITE);
    let parsed = parse_sip_uri(&r.uri).unwrap();
    assert_eq!(parsed.user.as_deref(), Some("bob"));
    assert_eq!(parsed.host, "example.com");
    assert_eq!(parsed.params.get("transport").map(String::as_str), Some("udp"));
    assert_eq!(parse_uri_params(&r.uri).get("transport").map(String::as_str), Some("udp"));
}

#[test]
fn call_id_cseq_maxforwards_content_type() {
    let m = parse(BASIC_INVITE);
    assert_eq!(get_header(m.headers(), "Call-ID"), Some("unique-call-id@10.0.0.1"));
    let SipMessage::Request(ref r) = m else { unreachable!() };
    assert_eq!(r.cseq.seq, 42);
    assert_eq!(r.cseq.method, "INVITE");
    assert_eq!(get_header(m.headers(), "Max-Forwards").unwrap().parse::<u32>().unwrap(), 70);
    assert_eq!(get_header(m.headers(), "Content-Type"), Some("application/sdp"));
}

// --- compact form expansion ---

#[test]
fn compact_forms_expand() {
    let m = parse(COMPACT_FORM);
    assert_eq!(parse_via_params(get_header(m.headers(), "Via").unwrap()).branch.as_deref(), Some("z9hG4bK-compact"));
    assert_eq!(extract_tag(get_header(m.headers(), "From").unwrap()), Some("compact-tag".to_string()));
    assert!(get_header(m.headers(), "To").is_some());
    assert_eq!(get_header(m.headers(), "Call-ID"), Some("compact-call-id"));
    assert_eq!(extract_contact_uri(get_header(m.headers(), "Contact").unwrap()), "sip:alice@10.0.0.1:5060");
    assert_eq!(get_header(m.headers(), "Content-Length"), Some("0"));
}

// --- header folding with extraction ---

#[test]
fn folded_via_from_contact() {
    let m = parse(FOLDED_HEADERS);
    let via_params = parse_via_params(get_header(m.headers(), "Via").unwrap());
    assert_eq!(via_params.branch.as_deref(), Some("z9hG4bK-fold"));
    assert_eq!(via_params.cr.as_deref(), Some("folded-ref"));
    assert_eq!(via_params.lg.as_deref(), Some("a"));
    assert_eq!(extract_tag(get_header(m.headers(), "From").unwrap()), Some("folded-from-tag".to_string()));
    let uri = extract_contact_uri(get_header(m.headers(), "Contact").unwrap());
    assert_eq!(uri, "sip:alice@10.0.0.1:5060;callRef=fold-ref;leg=a");
    let parsed = parse_sip_uri(&uri).unwrap();
    assert_eq!(parsed.params.get("callref").map(String::as_str), Some("fold-ref"));
    assert_eq!(parsed.params.get("leg").map(String::as_str), Some("a"));
}

// --- tag injection resistance: parsed fields are quote-aware ---

#[test]
fn tag_injection_resistance() {
    let cases: &[(&str, Option<&str>)] = &[
        (
            "From: \"Vincent ;tag=IamAhacker\" <sip:alice@example.com>;tag=real-tag\r\n",
            Some("real-tag"),
        ),
        (
            "From: \"Vincent <;tag=IamAhacker>\" <sip:alice@example.com>;tag=real-tag\r\n",
            Some("real-tag"),
        ),
        ("From: \"Vincent ;tag=IamAhacker\" <sip:alice@example.com>\r\n", None),
    ];
    for (from_line, expected) in cases {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inj\r\n\
{from_line}\
To: <sip:bob@example.com>\r\n\
Call-ID: injection@example.com\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        let r = req(&raw);
        // Eager parsed field: quote-aware.
        assert_eq!(r.from.tag.as_deref(), *expected, "parsed.from.tag for {from_line}");
        // The helper uses the same structured parser — also immune to injection.
        let from_header = get_header(&r.headers, "From").unwrap();
        assert_eq!(extract_tag(from_header).as_deref(), *expected, "extract_tag for {from_line}");
    }
}

// --- eager parsed fields ---

#[test]
fn parsed_from_to_basic() {
    let r = req(BASIC_INVITE);
    assert_eq!(r.from.display_name.as_deref(), Some("Alice Smith"));
    assert_eq!(r.from.uri, "sip:alice@example.com");
    assert_eq!(r.from.tag.as_deref(), Some("from-tag-xyz"));
    assert_eq!(r.to.uri, "sip:bob@example.com");
    assert_eq!(r.to.tag, None);
}

#[test]
fn parsed_via_fields() {
    let r = req(BASIC_INVITE);
    let top = r.via.first();
    assert_eq!(top.transport, "UDP");
    assert_eq!(top.host, "10.0.0.1");
    assert_eq!(top.port, Some(5060));
    assert_eq!(top.branch.as_deref(), Some("z9hG4bK-abc123"));
    assert_eq!(via_param(&top.params, "cr"), Some("call-ref-1"));
    assert_eq!(via_param(&top.params, "lg"), Some("a"));
}

#[test]
fn parsed_all_vias() {
    let r = req(MULTI_VIA);
    let vias: Vec<_> = r.via.iter().collect();
    assert_eq!(vias.len(), 3);
    assert_eq!(vias[0].branch.as_deref(), Some("z9hG4bK-hop3"));
    assert_eq!(vias[1].branch.as_deref(), Some("z9hG4bK-hop2"));
    assert_eq!(vias[2].branch.as_deref(), Some("z9hG4bK-hop1"));
    assert_eq!(via_param(&vias[0].params, "cr"), Some("ref"));
    assert_eq!(via_param(&vias[0].params, "lg"), Some("b-1"));
}

#[test]
fn parsed_contact_and_request_uri() {
    let r = req(BASIC_INVITE);
    assert_eq!(
        first_contact(&r.contacts).map(|c| c.uri.as_str()),
        Some("sip:alice@10.0.0.1:5060;callRef=call-ref-1;leg=a")
    );
    assert_eq!(r.request_uri.scheme, "sip");
    assert_eq!(r.request_uri.user.as_deref(), Some("bob"));
    assert_eq!(r.request_uri.host, "example.com");
    assert_eq!(r.request_uri.params.get("transport").map(String::as_str), Some("udp"));
}

#[test]
fn parsed_fields_with_compact_and_folded() {
    let c = req(COMPACT_FORM);
    assert_eq!(c.from.tag.as_deref(), Some("compact-tag"));
    assert_eq!(c.call_id, "compact-call-id");
    assert_eq!(c.via.first().branch.as_deref(), Some("z9hG4bK-compact"));
    assert_eq!(first_contact(&c.contacts).map(|x| x.uri.as_str()), Some("sip:alice@10.0.0.1:5060"));

    let f = req(FOLDED_HEADERS);
    assert_eq!(f.from.tag.as_deref(), Some("folded-from-tag"));
    assert_eq!(f.via.first().branch.as_deref(), Some("z9hG4bK-fold"));
    assert_eq!(via_param(&f.via.first().params, "cr"), Some("folded-ref"));
    assert_eq!(via_param(&f.via.first().params, "lg"), Some("a"));
    assert_eq!(
        first_contact(&f.contacts).map(|x| x.uri.as_str()),
        Some("sip:alice@10.0.0.1:5060;callRef=fold-ref;leg=a")
    );
}

#[test]
fn semicolon_in_user_portion_is_accepted() {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-user-semi\r\n\
From: \"toto\" <sip:+33123456789;titi=tat@foo.bar>;tag=from-tag-1\r\n\
To: \"tutu\" <sip:+33198765432;npi=e164@bar.baz>\r\n\
Call-ID: user-semi-1@example.com\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let r = req(raw);
    assert_eq!(r.from.uri, "sip:+33123456789;titi=tat@foo.bar");
    assert_eq!(r.from.display_name.as_deref(), Some("toto"));
    assert_eq!(r.from.tag.as_deref(), Some("from-tag-1"));
    assert_eq!(r.to.uri, "sip:+33198765432;npi=e164@bar.baz");
    assert_eq!(r.to.display_name.as_deref(), Some("tutu"));
}

// --- Refer-To: `?` in userinfo vs embedded-headers boundary ---

#[test]
fn refer_to_question_mark_in_userinfo_vs_embedded_headers() {
    let refer_value = "<sips:+33?param=v@host.example;lr?Replaces=abc%40d%3Bfrom-tag%3D1%3Bto-tag%3D2>";
    let raw = format!(
        "REFER sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-refer-q\r\n\
From: <sip:alice@example.com>;tag=t1\r\n\
To: <sip:bob@example.com>;tag=t2\r\n\
Call-ID: refer-q-1@example.com\r\n\
CSeq: 1 REFER\r\n\
Refer-To: {refer_value}\r\n\
Content-Length: 0\r\n\r\n"
    );
    let r = req(&raw);
    let refer_to = r
        .optional
        .refer_to
        .as_ref()
        .expect("Refer-To parses")
        .as_ref()
        .expect("Refer-To present");
    assert_eq!(refer_to.uri, "sips:+33?param=v@host.example;lr?Replaces=abc%40d%3Bfrom-tag%3D1%3Bto-tag%3D2");
    assert_eq!(refer_to.embedded_headers.keys().collect::<Vec<_>>(), vec!["Replaces"]);
    let replaces = refer_to.replaces.as_ref().expect("Replaces parsed");
    assert_eq!(replaces.call_id, "abc@d");
    assert_eq!(replaces.from_tag, "1");
    assert_eq!(replaces.to_tag, "2");
}
