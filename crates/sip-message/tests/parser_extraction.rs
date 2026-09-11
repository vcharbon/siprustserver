//! Parser extraction — raw bytes → parse → structured data, exercised through
//! the downstream header helpers and the eager typed fields. Port of the
//! custom-parser sections of `tests/sip/parser-extraction.test.ts`.
//!
//! The cross-parser "custom vs JsSIP" equivalence block is NOT ported — there
//! is no JsSIP in the Rust stack (ADR-0001); the `rvoip` parity oracle takes
//! that role in the compliance matrix.

use sip_message::header::{Contact, HeaderName, HeaderValue, Params, To, Uri, Via};
use sip_message::{ContactSet, CustomParser, SipMessage, SipParser, SipRequest, SipStr};

fn name(header: &str) -> HeaderName {
    HeaderName::of(&SipStr::owned(header))
}

/// First value of one header.
fn first_value<'a>(msg: &'a SipMessage, header: &str) -> Option<&'a str> {
    msg.raw(name(header)).next()
}

/// Every value of one header, in wire order.
fn all_values<'a>(msg: &'a SipMessage, header: &str) -> Vec<&'a str> {
    msg.raw(name(header)).collect()
}

/// The dialog tag a From/To value carries.
fn tag_of(value: &str) -> Option<String> {
    To::parse(&SipStr::owned(value)).ok()?.tag().map(str::to_owned)
}

/// The same address without its tag.
fn untagged(value: &str) -> String {
    To::parse(&SipStr::owned(value)).expect("readable address").without_tag().to_wire()
}

/// The URI a Contact names.
fn contact_uri(value: &str) -> Uri {
    Contact::parse(&SipStr::owned(value)).expect("readable Contact").into_addr().uri().clone()
}

/// One Via line, read.
fn via_of(value: &str) -> Via {
    Via::parse(&SipStr::owned(value)).expect("readable Via")
}

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

fn via_param<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params.value(key)
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
    let from = first_value(&m, "From").unwrap();
    assert_eq!(tag_of(from), Some("from-tag-xyz".to_string()));
    let stripped = untagged(from);
    assert!(!stripped.contains("tag="));
    assert!(stripped.contains("sip:alice@example.com"));
}

#[test]
fn to_tag_extraction() {
    assert_eq!(tag_of(first_value(&parse(BASIC_INVITE), "To").unwrap()), None);
    assert_eq!(
        tag_of(first_value(&parse(RESPONSE_WITH_TAGS), "To").unwrap()),
        Some("to-tag-2".to_string())
    );
}

#[test]
fn via_branch_cr_lg_extraction() {
    let m = parse(BASIC_INVITE);
    let top = all_values(&m, "Via")[0];
    let p = via_of(top);
    assert_eq!(p.branch(), Some("z9hG4bK-abc123"));
    assert_eq!(p.params().value("cr"), Some("call-ref-1"));
    assert_eq!(p.params().value("lg"), Some("a"));
}

#[test]
fn multiple_via_order_preserved() {
    let m = parse(MULTI_VIA);
    let vias = all_values(&m, "Via");
    assert_eq!(vias.len(), 3);
    assert_eq!(via_of(vias[0]).branch(), Some("z9hG4bK-hop3"));
    assert_eq!(via_of(vias[1]).branch(), Some("z9hG4bK-hop2"));
    assert_eq!(via_of(vias[2]).branch(), Some("z9hG4bK-hop1"));
}

#[test]
fn contact_uri_and_params_extraction() {
    let m = parse(BASIC_INVITE);
    let contact = first_value(&m, "Contact").unwrap();
    let uri = contact_uri(contact);
    assert_eq!(uri.text(), "sip:alice@10.0.0.1:5060;callRef=call-ref-1;leg=a");
    assert_eq!(uri.host(), "10.0.0.1");
    assert_eq!(uri.host_port(), ("10.0.0.1", 5060));
    assert_eq!(uri.user(), Some("alice"));
    // Parameter names keep their wire spelling; lookups are case-insensitive.
    assert_eq!(uri.params().value("callref"), Some("call-ref-1"));
    assert_eq!(uri.params().value("leg"), Some("a"));
}

#[test]
fn request_uri_parsing_and_params() {
    let r = req(BASIC_INVITE);
    let parsed = r.request_uri();
    assert_eq!(parsed.user(), Some("bob"));
    assert_eq!(parsed.host(), "example.com");
    assert_eq!(parsed.params().value("transport"), Some("udp"));
}

#[test]
fn call_id_cseq_maxforwards_content_type() {
    let m = parse(BASIC_INVITE);
    assert_eq!(first_value(&m, "Call-ID"), Some("unique-call-id@10.0.0.1"));
    let SipMessage::Request(ref r) = m else { unreachable!() };
    assert_eq!(r.cseq().seq(), 42);
    assert_eq!(r.cseq().method().as_str(), "INVITE");
    assert_eq!(first_value(&m, "Max-Forwards").unwrap().parse::<u32>().unwrap(), 70);
    assert_eq!(first_value(&m, "Content-Type"), Some("application/sdp"));
}

// --- compact form expansion ---

#[test]
fn compact_forms_expand() {
    let m = parse(COMPACT_FORM);
    assert_eq!(via_of(first_value(&m, "Via").unwrap()).branch(), Some("z9hG4bK-compact"));
    assert_eq!(tag_of(first_value(&m, "From").unwrap()), Some("compact-tag".to_string()));
    assert!(first_value(&m, "To").is_some());
    assert_eq!(first_value(&m, "Call-ID"), Some("compact-call-id"));
    assert_eq!(contact_uri(first_value(&m, "Contact").unwrap()).text(), "sip:alice@10.0.0.1:5060");
    assert_eq!(first_value(&m, "Content-Length"), Some("0"));
}

// --- header folding with extraction ---

#[test]
fn folded_via_from_contact() {
    let m = parse(FOLDED_HEADERS);
    let via_params = via_of(first_value(&m, "Via").unwrap());
    assert_eq!(via_params.branch(), Some("z9hG4bK-fold"));
    assert_eq!(via_params.params().value("cr"), Some("folded-ref"));
    assert_eq!(via_params.params().value("lg"), Some("a"));
    assert_eq!(tag_of(first_value(&m, "From").unwrap()), Some("folded-from-tag".to_string()));
    let uri = contact_uri(first_value(&m, "Contact").unwrap());
    assert_eq!(uri.text(), "sip:alice@10.0.0.1:5060;callRef=fold-ref;leg=a");
    assert_eq!(uri.params().value("callref"), Some("fold-ref"));
    assert_eq!(uri.params().value("leg"), Some("a"));
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
        assert_eq!(r.from().tag(), *expected, "parsed.from.tag for {from_line}");
        // The helper uses the same structured parser — also immune to injection.
        let parsed = parse(&raw);
        let from_header = first_value(&parsed, "From").unwrap();
        assert_eq!(tag_of(from_header).as_deref(), *expected, "tag_of for {from_line}");
    }
}

// --- eager parsed fields ---

#[test]
fn parsed_from_to_basic() {
    let r = req(BASIC_INVITE);
    assert_eq!(r.from().display(), Some("Alice Smith"));
    assert_eq!(r.from().uri().text(), "sip:alice@example.com");
    assert_eq!(r.from().tag(), Some("from-tag-xyz"));
    assert_eq!(r.to().uri().text(), "sip:bob@example.com");
    assert_eq!(r.to().tag(), None);
}

#[test]
fn parsed_via_fields() {
    let r = req(BASIC_INVITE);
    let top = r.via().first();
    assert_eq!(top.transport(), "UDP");
    assert_eq!(top.host(), "10.0.0.1");
    assert_eq!(top.port(), Some(5060));
    assert_eq!(top.branch(), Some("z9hG4bK-abc123"));
    assert_eq!(via_param(top.params(), "cr"), Some("call-ref-1"));
    assert_eq!(via_param(top.params(), "lg"), Some("a"));
}

#[test]
fn parsed_all_vias() {
    let r = req(MULTI_VIA);
    let vias: Vec<_> = r.via().iter().collect();
    assert_eq!(vias.len(), 3);
    assert_eq!(vias[0].branch(), Some("z9hG4bK-hop3"));
    assert_eq!(vias[1].branch(), Some("z9hG4bK-hop2"));
    assert_eq!(vias[2].branch(), Some("z9hG4bK-hop1"));
    assert_eq!(via_param(vias[0].params(), "cr"), Some("ref"));
    assert_eq!(via_param(vias[0].params(), "lg"), Some("b-1"));
}

#[test]
fn parsed_contact_and_request_uri() {
    let r = req(BASIC_INVITE);
    assert_eq!(
        first_contact(r.contacts()).and_then(|c| c.uri().source()),
        Some("sip:alice@10.0.0.1:5060;callRef=call-ref-1;leg=a")
    );
    assert_eq!(r.request_uri().scheme(), "sip");
    assert_eq!(r.request_uri().user(), Some("bob"));
    assert_eq!(r.request_uri().host(), "example.com");
    assert_eq!(r.request_uri().params().value("transport"), Some("udp"));
}

#[test]
fn parsed_fields_with_compact_and_folded() {
    let c = req(COMPACT_FORM);
    assert_eq!(c.from().tag(), Some("compact-tag"));
    assert_eq!(c.call_id().as_str(), "compact-call-id");
    assert_eq!(c.via().first().branch(), Some("z9hG4bK-compact"));
    assert_eq!(
        first_contact(c.contacts()).and_then(|x| x.uri().source()),
        Some("sip:alice@10.0.0.1:5060")
    );

    let f = req(FOLDED_HEADERS);
    assert_eq!(f.from().tag(), Some("folded-from-tag"));
    assert_eq!(f.via().first().branch(), Some("z9hG4bK-fold"));
    assert_eq!(via_param(f.via().first().params(), "cr"), Some("folded-ref"));
    assert_eq!(via_param(f.via().first().params(), "lg"), Some("a"));
    assert_eq!(
        first_contact(f.contacts()).and_then(|x| x.uri().source()),
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
    assert_eq!(r.from().uri().text(), "sip:+33123456789;titi=tat@foo.bar");
    assert_eq!(r.from().display(), Some("toto"));
    assert_eq!(r.from().tag(), Some("from-tag-1"));
    assert_eq!(r.to().uri().text(), "sip:+33198765432;npi=e164@bar.baz");
    assert_eq!(r.to().display(), Some("tutu"));
}

// --- Refer-To: `?` in userinfo vs embedded-headers boundary ---

#[test]
fn refer_to_question_mark_in_userinfo_vs_embedded_headers() {
    let refer_value =
        "<sips:+33?param=v@host.example;lr?Replaces=abc%40d%3Bfrom-tag%3D1%3Bto-tag%3D2>";
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
        .optional()
        .refer_to
        .as_ref()
        .expect("Refer-To parses")
        .as_ref()
        .expect("Refer-To present");
    assert_eq!(
        refer_to.uri().text(),
        "sips:+33?param=v@host.example;lr?Replaces=abc%40d%3Bfrom-tag%3D1%3Bto-tag%3D2"
    );
    assert_eq!(
        refer_to.uri().escaped_headers().map(|(k, _)| k).collect::<Vec<_>>(),
        vec!["Replaces"]
    );
    // The `?` before the authority is userinfo, so only the SECOND `?` opens
    // the embedded-header list — and its value percent-decodes whole.
    assert_eq!(
        refer_to.uri().escaped_header("Replaces").as_deref(),
        Some("abc@d;from-tag=1;to-tag=2")
    );
}
