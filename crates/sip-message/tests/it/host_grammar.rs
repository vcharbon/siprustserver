//! The host grammar a message is read with:
//!
//! - an IPv6 reference holds an RFC 4291 text address (RFC 5954 §4.1 replaces
//!   the RFC 3261 `IPv6address`), wherever a host is read;
//! - the Via sent-by is read by the URI's host reader, on the wire and typed;
//! - a URI of a scheme other than sip, sips or tel is opaque;
//! - a `received` is stamped when the source address differs from the sent-by;
//! - the RFC 6442 headers keep what they were written with.

use sip_message::header::{
    network_host, Geolocation, GeolocationError, HeaderValue, ParamValue, RouteEntry, Uri, Via,
};
use sip_message::remote_target::rewrite_hostport;
use sip_message::{CustomParser, SipMessage, SipParser, SipParserLimits, SipStr};

/// Text that RFC 3261's `hexseq` admits and no RFC 4291 address is.
const NOT_ADDRESSES: [&str; 2] = ["1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8::"];

/// RFC 4291 §2.2 text forms, an embedded IPv4 address and a leading-zero
/// group among them.
const ADDRESSES: [&str; 3] = ["::ffff:192.0.2.1", "fdaf:1f32:3::0341", "2001:db8::1"];

/// An INVITE with its Request-URI and top Via replaced, and `extra` header
/// lines added.
fn invite(request_uri: &str, via: &str, extra: &str) -> String {
    format!(
        "INVITE {request_uri} SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>\r\n\
         Call-ID: host-grammar-1@192.0.2.1\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:alice@192.0.2.1:5060>\r\n{extra}Content-Length: 0\r\n\r\n"
    )
}

const VIA: &str = "SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-hg1";

fn parse(raw: &str) -> Result<SipMessage, String> {
    CustomParser::new().parse(raw.as_bytes()).map_err(|e| e.reason)
}

fn with_via(via: &str) -> Result<SipMessage, String> {
    parse(&invite("sip:bob@example.com", via, ""))
}

fn uri(text: &str) -> Result<Uri, String> {
    Uri::parse(&SipStr::owned(text)).map_err(|e| e.reason)
}

fn typed_via(text: &str) -> Result<Via, String> {
    Via::parse(&SipStr::owned(text)).map_err(|e| e.reason)
}

// ---------------------------------------------------------------------------
// IPv6 references
// ---------------------------------------------------------------------------

#[test]
fn an_ipv6_reference_holds_an_rfc_4291_address_only() {
    for bad in NOT_ADDRESSES {
        assert!(uri(&format!("sip:bob@[{bad}]")).is_err(), "URI host [{bad}] accepted");
        let contact = format!("Contact: <sip:bob@[{bad}]:5060>\r\n");
        let raw = invite("sip:bob@example.com", VIA, &contact)
            .replace("Contact: <sip:alice@192.0.2.1:5060>\r\n", "");
        assert!(parse(&raw).is_err(), "Contact host [{bad}] accepted");
        let param = format!("Record-Route: <sip:p.example.com;lr>;x=[{bad}]\r\n");
        assert!(
            parse(&invite("sip:bob@example.com", VIA, &param)).is_err(),
            "header parameter [{bad}] accepted"
        );
        let via = format!("SIP/2.0/UDP [{bad}]:5060;branch=z9hG4bK-hg1");
        assert!(with_via(&via).is_err(), "wire Via sent-by [{bad}] accepted");
        assert!(typed_via(&via).is_err(), "typed Via sent-by [{bad}] accepted");
    }
}

#[test]
fn an_rfc_4291_address_reads_in_every_host_position() {
    for good in ADDRESSES {
        let u = uri(&format!("sip:bob@[{good}]:5070")).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!((u.host(), u.port()), (good, Some(5070)));
        let param = format!("Record-Route: <sip:p.example.com;lr>;x=[{good}]\r\n");
        parse(&invite("sip:bob@example.com", VIA, &param)).unwrap_or_else(|e| panic!("{e}"));
        let via = format!("SIP/2.0/UDP [{good}]:5070;branch=z9hG4bK-hg1");
        let msg = with_via(&via).unwrap_or_else(|e| panic!("{via}: {e}"));
        assert_eq!(msg.via().first().host_port(), (good, 5070));
        let typed = typed_via(&via).unwrap_or_else(|e| panic!("{via}: {e}"));
        assert_eq!(typed.host_port(), (good, 5070));
    }
}

// ---------------------------------------------------------------------------
// The Via sent-by
// ---------------------------------------------------------------------------

#[test]
fn a_via_sent_by_bracket_holds_an_ipv6_address_only() {
    for bad in ["[zz]", "[example.com]", "[192.0.2.1]", "[]", "[2001:db8::1]x", "[2001:db8::1"] {
        let via = format!("SIP/2.0/UDP {bad};branch=z9hG4bK-hg1");
        let reason = with_via(&via).expect_err(&via);
        assert!(reason.contains("Via sent-by"), "{via}: refused for {reason:?}");
        assert!(typed_via(&via).is_err(), "typed {via} accepted");
    }
}

#[test]
fn only_a_port_a_parameter_or_a_separator_follows_the_sent_by_host() {
    for bad in [
        "SIP/2.0/UDP example.com x;branch=z9hG4bK-hg1",
        "SIP/2.0/UDP [2001:db8::1]:5060 x;branch=z9hG4bK-hg1",
        "SIP/2.0/UDP example.com:;branch=z9hG4bK-hg1",
    ] {
        assert!(with_via(bad).is_err(), "wire {bad} accepted");
        assert!(typed_via(bad).is_err(), "typed {bad} accepted");
    }
    for (good, host, port) in [
        ("SIP/2.0/UDP [2001:db8::1] ;branch=z9hG4bK-hg1", "2001:db8::1", None),
        ("SIP/2.0/UDP example.com : 5070 ;branch=z9hG4bK-hg1", "example.com", Some(5070)),
        ("SIP/2.0/UDP example.com\t;branch=z9hG4bK-hg1", "example.com", None),
    ] {
        let msg = with_via(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        let top = msg.via().first();
        assert_eq!((top.host(), top.port()), (host, port), "{good}");
        let typed = typed_via(good).unwrap_or_else(|e| panic!("typed {good}: {e}"));
        assert_eq!((typed.host(), typed.port()), (host, port), "typed {good}");
    }
}

#[test]
fn a_bracketed_or_bare_received_still_reads() {
    for received in ["2001:db8::2", "[2001:db8::2]"] {
        let via = format!("SIP/2.0/UDP [2001:db8::1]:5060;branch=z9hG4bK-hg1;received={received}");
        let msg = with_via(&via).unwrap_or_else(|e| panic!("{via}: {e}"));
        assert_eq!(msg.via().first().received(), Some(received));
    }
}

#[test]
fn a_received_is_stamped_on_an_address_difference_only() {
    let via = |sent_by: &str| {
        typed_via(&format!("SIP/2.0/UDP {sent_by};branch=z9hG4bK-hg1")).expect("a Via")
    };
    for sent_by in ["[2001:DB8::1]:5060", "[2001:db8:0:0::1]:5060"] {
        let stamped = via(sent_by).stamped_from("2001:db8::1", 5060);
        assert_eq!(stamped.received(), None, "{sent_by} is the source address");
    }
    let moved = via("[2001:db8::1]:5060").stamped_from("2001:db8::2", 5060);
    assert_eq!(moved.received(), Some("2001:db8::2"));
    let named = via("host.example.com").stamped_from("192.0.2.7", 5060);
    assert_eq!(named.received(), Some("192.0.2.7"));
    let mapped = via("192.0.2.1:5060").stamped_from("::ffff:192.0.2.1", 5060);
    assert_eq!(mapped.received(), None, "an IPv4-mapped source is the IPv4 address");
}

/// A message read leniently (`wire_grammar = false`) never fails on its
/// sent-by: one the reader refuses is held whole, with no port, and renders
/// back as written.
#[test]
fn a_lenient_reading_holds_a_refused_sent_by_whole() {
    let lenient = CustomParser::with_limits(SipParserLimits {
        wire_grammar: false,
        ..SipParserLimits::default()
    });
    for sent_by in [
        "[2001:db8::00001]",
        "[1:2:3:4:5:6:7::8]",
        "192.0.2.1:",
        "[2001:db8::1]x",
        "192.0.2.1:5060abc",
    ] {
        let via = format!("SIP/2.0/UDP {sent_by};branch=z9hG4bK-hg1");
        let raw = invite("sip:bob@example.com", &via, "");
        let msg = lenient.parse(raw.as_bytes()).unwrap_or_else(|e| panic!("{via}: {}", e.reason));
        let top = msg.via().first();
        assert_eq!((top.host(), top.port()), (sent_by, None), "{via}");
        assert_eq!(top.to_wire(), via);
        assert!(with_via(&via).is_err(), "the wire refuses {via}");
    }
}

// ---------------------------------------------------------------------------
// URIs of another scheme
// ---------------------------------------------------------------------------

#[test]
fn a_uri_of_another_scheme_is_opaque() {
    for (text, scheme) in [
        ("urn:service:sos", "urn"),
        ("urn:nena:service:sos", "urn"),
        ("urn:ietf:params:xml:ns:pidf", "urn"),
        ("http://example.com/a:b", "http"),
        ("http://media.example.com/ring.wav", "http"),
        ("https://[2001:db8::1]:8443/loc", "https"),
        ("cid:loc@example.com", "cid"),
    ] {
        let u = uri(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(u.scheme(), scheme, "{text}");
        assert_eq!((u.user(), u.host(), u.port()), (None, "", None), "{text}");
        assert!(u.params().is_empty(), "{text}");
        assert_eq!(u.escaped_headers().count(), 0, "{text}");
        assert_eq!(u.text(), text);
        assert_eq!(u.clone().normalized().to_string(), text, "{text} rendered from its parts");
    }
    assert_ne!(uri("urn:service:sos"), uri("urn:service:police"));
    assert!(uri("urn:service:s\u{f6}s").is_err(), "a URI byte outside the URI character set");
}

/// RFC 3261 §25.1 `opaque-part = uric-no-slash *uric` and RFC 3966
/// `telephone-subscriber` are never empty.
#[test]
fn an_empty_scheme_specific_part_is_no_uri() {
    for empty in ["urn:", "cid:", "tel:", "tel:;phone-context=example.com"] {
        assert!(uri(empty).is_err(), "{empty:?} accepted");
    }
    let contact =
        invite("sip:bob@example.com", VIA, "").replace("<sip:alice@192.0.2.1:5060>", "<urn:>");
    assert!(parse(&contact).is_err(), "Contact <urn:> accepted");
}

#[test]
fn tel_keeps_its_number_and_parameters() {
    let u = uri("tel:+1-555-0100;phone-context=example.com").expect("a tel URI");
    assert_eq!(u.host(), "+1-555-0100");
    assert_eq!(u.param("phone-context"), Some(&ParamValue::Token("example.com".into())));
    assert_eq!(u.to_string(), "tel:+1-555-0100;phone-context=example.com");
}

#[test]
fn a_request_uri_or_contact_of_another_scheme_is_read() {
    let raw = invite("urn:nena:service:sos", VIA, "");
    let msg = parse(&raw).unwrap_or_else(|e| panic!("urn Request-URI: {e}"));
    let SipMessage::Request(req) = msg else { panic!("a request") };
    assert_eq!(req.request_uri().to_string(), "urn:nena:service:sos");
    let contact = invite("sip:bob@example.com", VIA, "")
        .replace("<sip:alice@192.0.2.1:5060>", "<urn:ietf:params:x:y>");
    parse(&contact).unwrap_or_else(|e| panic!("urn Contact: {e}"));
}

/// A route is a hop a request is sent to: a URI that names no host is none.
#[test]
fn a_route_of_another_scheme_names_no_hop() {
    for line in ["Route: <urn:service:sos>\r\n", "Record-Route: <http://example.com/a>\r\n"] {
        assert!(parse(&invite("sip:bob@example.com", VIA, line)).is_err(), "{line:?} accepted");
    }
    assert!(RouteEntry::parse(&SipStr::owned("<urn:service:sos>")).is_err());
    assert!(RouteEntry::parse(&SipStr::owned("<sip:p.example.com;lr>")).is_ok());
}

// ---------------------------------------------------------------------------
// RFC 6442 location headers
// ---------------------------------------------------------------------------

fn located(extra: &str) -> SipMessage {
    parse(&invite("sip:bob@example.com", VIA, extra)).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn an_https_location_with_an_ipv6_authority_reads() {
    let msg = located("Geolocation: <https://[2001:db8::1]:8443/loc>\r\n");
    let list = msg.list::<Geolocation>().unwrap_or_else(|e| panic!("{}", e.reason));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uri().text(), "https://[2001:db8::1]:8443/loc");
    assert_eq!(network_host(&list[0].uri().text()), Some("[2001:db8::1]"));
}

#[test]
fn an_empty_location_entry_is_skipped() {
    for (line, count) in [
        ("Geolocation: <cid:a@example.com>, , <cid:c@example.com>\r\n", 2),
        ("Geolocation: <cid:a@example.com>,\r\n", 1),
    ] {
        let list = located(line).list::<Geolocation>().unwrap_or_else(|e| panic!("{}", e.reason));
        assert_eq!(list.len(), count, "{line:?}");
    }
}

#[test]
fn a_location_error_code_keeps_its_digits() {
    for (code, value) in [("000", 0), ("010", 10)] {
        let msg = located(&format!("Geolocation-Error: {code};code=\"x\"\r\n"));
        let error = msg.header::<GeolocationError>().expect("present").expect("reads");
        assert_eq!(error.code(), value);
        assert_eq!(error.to_wire(), format!("{code};code=\"x\""));
    }
}

#[test]
fn a_hostport_rewrite_keeps_the_scheme_as_written() {
    assert_eq!(
        rewrite_hostport("<SIP:alice@example.com:5060>;expires=60", "192.0.2.1", 5070),
        "<SIP:alice@192.0.2.1:5070>;expires=60"
    );
    assert_eq!(
        rewrite_hostport("<Sips:alice@example.com>", "192.0.2.1", 5061),
        "<Sips:alice@192.0.2.1:5061>"
    );
}
