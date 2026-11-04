//! The RFC 3261 §25.1 grammar on the wire: the URI of a Request-URI, From, To,
//! Contact, Route and Record-Route is read by one reader that refuses what the
//! grammar refuses and routes on what it validated; the name-addr around it
//! keeps its brackets and nothing after `>` but parameters; the other elements
//! the parser reads keep their character classes. Each refusal sits beside a
//! well-formed counterpart that must keep parsing.

use sip_message::header::{NameAddr, Uri, UriRole};
use sip_message::{CustomParser, SipMessage, SipParser, SipRequest, SipStr};

/// An INVITE with one line replaced by `line` (matched on its header name, or the
/// request line when `name` is `"INVITE"`), and `extra` added before the end.
fn invite(name: &str, line: &str, extra: &str) -> String {
    let lines = [
        ("INVITE", "INVITE sip:bob@example.com SIP/2.0".to_string()),
        ("Via", "Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-uri1".to_string()),
        ("Max-Forwards", "Max-Forwards: 70".to_string()),
        ("From", "From: Alice <sip:alice@example.com>;tag=a1".to_string()),
        ("To", "To: <sip:bob@example.com>".to_string()),
        ("Call-ID", "Call-ID: uri-grammar-1@192.0.2.1".to_string()),
        ("CSeq", "CSeq: 1 INVITE".to_string()),
        ("Contact", "Contact: <sip:alice@192.0.2.1:5060>".to_string()),
    ];
    let mut out = String::new();
    for (n, l) in lines {
        out.push_str(if n == name { line } else { &l });
        out.push_str("\r\n");
    }
    out.push_str(extra);
    out.push_str("Content-Length: 0\r\n\r\n");
    out
}

fn parse(raw: &str) -> Result<SipMessage, String> {
    CustomParser::new().parse(raw.as_bytes()).map_err(|e| e.reason)
}

fn request(raw: &str) -> SipRequest {
    match parse(raw) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("expected a request from {raw:?}: {other:?}"),
    }
}

fn refused(what: &str, raw: &str) {
    assert!(parse(raw).is_err(), "{what}: accepted {raw:?}");
}

fn uri(text: &str) -> Result<Uri, String> {
    Uri::parse(&SipStr::owned(text)).map_err(|e| e.reason)
}

// ---------------------------------------------------------------------------
// One reader: the URI that is validated is the URI that is routed
// ---------------------------------------------------------------------------

#[test]
fn a_request_uri_the_grammar_refuses_is_never_routed() {
    for r_uri in [
        "sip:a>b@example.com",
        "sip:bob@example.com,evil.example.org",
        "sip:bob@example.com>junk",
        "sip:bob@example.com\tjunk",
        "sip:bob@exa\"mple.com",
    ] {
        refused("Request-URI", &invite("INVITE", &format!("INVITE {r_uri} SIP/2.0"), ""));
        assert!(uri(r_uri).is_err(), "the typed reader takes {r_uri:?}");
    }
}

#[test]
fn a_request_uri_carries_no_escaped_headers() {
    refused(
        "Request-URI ?headers (RFC 3261 §19.1.1 Table 1)",
        &invite("INVITE", "INVITE sip:bob@example.com?Subject=hi SIP/2.0", ""),
    );
}

#[test]
fn a_well_formed_request_uri_routes_on_every_part() {
    let r = request(&invite(
        "INVITE",
        "INVITE sip:+33612345678;npdi@example.com:5070;user=phone;lr SIP/2.0",
        "",
    ));
    let u = r.request_uri();
    assert_eq!(u.user(), Some("+33612345678;npdi"));
    assert_eq!(u.host_port(), ("example.com", 5070));
    assert_eq!(u.param("user").and_then(|v| v.as_str()), Some("phone"));
    assert!(u.is_loose_route());
}

// ---------------------------------------------------------------------------
// Raw UTF-8 in a URI, any position, any scheme
// ---------------------------------------------------------------------------

#[test]
fn raw_utf8_in_a_uri_is_refused_everywhere() {
    for r_uri in ["sip:zo\u{eb}@example.com", "sip:zoe@exampl\u{eb}.com", "tel:+1555\u{eb}"] {
        refused("raw UTF-8 Request-URI", &invite("INVITE", &format!("INVITE {r_uri} SIP/2.0"), ""));
        assert!(uri(r_uri).is_err(), "the typed reader takes {r_uri:?}");
    }
    refused("raw UTF-8 From", &invite("From", "From: <sip:zo\u{eb}@example.com>;tag=a1", ""));
    refused("raw UTF-8 To", &invite("To", "To: <sip:zoe@exampl\u{eb}.com>", ""));
    refused("raw UTF-8 Contact", &invite("Contact", "Contact: <sip:zo\u{eb}@192.0.2.1>", ""));
    refused("raw UTF-8 Route", &invite("", "", "Route: <sip:zo\u{eb}@192.0.2.9;lr>\r\n"));
}

#[test]
fn escaped_utf8_and_a_quoted_utf8_display_name_keep_parsing() {
    let r =
        request(&invite("From", "From: \"Zo\u{eb} Fakey\" <sip:zo%C3%AB@example.com>;tag=a1", ""));
    assert_eq!(r.from().display(), Some("Zo\u{eb} Fakey"));
    assert_eq!(r.from().uri().user(), Some("zo%C3%AB"));
    let r = request(&invite("INVITE", "INVITE sip:zo%C3%AB@example.com SIP/2.0", ""));
    assert_eq!(r.request_uri().user(), Some("zo%C3%AB"));
}

// ---------------------------------------------------------------------------
// The sip/sips grammar, part by part
// ---------------------------------------------------------------------------

#[test]
fn userinfo_takes_only_its_characters_and_well_formed_escapes() {
    for bad in [
        "sip:a b@example.com",
        "sip:a\"b@example.com",
        "sip:a<b@example.com",
        "sip:a%G1@example.com",
        "sip:a%@example.com",
        "sip:a%0@example.com",
        "sip:@example.com",
        "sip:a@b@example.com",
        "sip:a:p\"w@example.com",
    ] {
        assert!(uri(bad).is_err(), "userinfo {bad:?} accepted");
        refused("From userinfo", &invite("From", &format!("From: <{bad}>;tag=a1"), ""));
    }
    for good in [
        "sip:a%20b@example.com",
        "sip:%00@example.com",
        "sip:a&=+$,;?/b@example.com",
        "sip:a:p&w@h",
    ] {
        assert!(uri(good).is_ok(), "userinfo {good:?} refused: {:?}", uri(good));
    }
}

#[test]
fn host_labels_follow_the_hostname_and_ipv4_grammar() {
    for bad in [
        "sip:a@h_x.example.com",
        "sip:a@a-.example.com",
        "sip:a@-a.example.com",
        "sip:a@h%41.example.com",
        "sip:a@1.2.3",
        "sip:a@1.2.3.4.5",
        "sip:a@1.2.3.4.",
        "sip:a@example..com",
        "sip:a@256.1.1.1",
        "sip:a@27-.example",
    ] {
        assert!(uri(bad).is_err(), "host {bad:?} accepted");
        refused("To host", &invite("To", &format!("To: <{bad}>"), ""));
    }
    for good in ["sip:a@example.com.", "sip:a@a-b.c9.example.com", "sip:a@192.0.2.1", "sip:a@x"] {
        assert!(uri(good).is_ok(), "host {good:?} refused: {:?}", uri(good));
    }
}

/// The one leniency of the host grammar, where a host names a party and is
/// never resolved (From, To, the identity headers): a top label need not start
/// with a letter (RFC 1123 §2.1). A dotted run of digits is still an IPv4
/// address or nothing.
#[test]
fn a_digit_led_top_label_names_a_party() {
    let r = request(&invite("From", "From: <sip:+33612345678@27:5790;user=phone>;tag=a1", ""));
    assert_eq!(r.from().uri().host_port(), ("27", 5790));
    let r = request(&invite("To", "To: <sip:bob@2130706433>", ""));
    assert_eq!(r.to().uri().host(), "2130706433");
    let r = request(&invite("", "", "P-Asserted-Identity: <sip:+33612345678@27>\r\n"));
    assert!(r.optional().p_asserted_identity.is_ok(), "{:?}", r.optional().p_asserted_identity);
    for good in ["sip:a@example.123", "sip:a@1x.27", "sip:a@27", "sip:a@9-9.27."] {
        let identity = Uri::parse_as(&SipStr::owned(good), UriRole::Identity);
        assert!(identity.is_ok(), "host {good:?} refused: {identity:?}");
    }
}

/// A host a request is sent to or routed on keeps the RFC 3261 top label: a
/// resolver reads `2130706433`, `0x7f.0.0.1` or `0177` as an IPv4 literal.
#[test]
fn a_resolved_host_keeps_the_rfc_3261_top_label() {
    for host in ["2130706433", "0x7f.0.0.1", "0177", "1x.27"] {
        let target = format!("sip:bob@{host}");
        assert!(uri(&target).is_err(), "Request-URI host {host:?} accepted");
        refused("Request-URI", &invite("INVITE", &format!("INVITE {target} SIP/2.0"), ""));
        refused("Route", &invite("", "", &format!("Route: <sip:{host};lr>\r\n")));
        refused("Record-Route", &invite("", "", &format!("Record-Route: <sip:{host};lr>\r\n")));
        refused("Contact", &invite("Contact", &format!("Contact: <{target}>"), ""));
        let via = format!("Via: SIP/2.0/UDP {host}:5060;branch=z9hG4bK-uri1");
        refused("Via", &invite("Via", &via, ""));
    }
}

/// RFC 3261 §7.1 and §25.1: a method is case-sensitive, so an extension
/// method keeps the case it was written in, on the request line and in the
/// CSeq.
#[test]
fn an_extension_method_keeps_its_case() {
    let raw = invite("INVITE", "Foo-Bar sip:bob@example.com SIP/2.0", "")
        .replace("CSeq: 1 INVITE", "CSeq: 1 Foo-Bar");
    let r = request(&raw);
    assert_eq!(r.method().as_str(), "Foo-Bar");
    assert_eq!(r.cseq().method().as_str(), "Foo-Bar");
}

/// The CSeq names the request's own method (§8.1.1.5) in its own case
/// (§7.1): `invite` is an extension method, no INVITE.
#[test]
fn a_method_matches_its_cseq_in_case() {
    let raw = invite("INVITE", "Foo-Bar sip:bob@example.com SIP/2.0", "")
        .replace("CSeq: 1 INVITE", "CSeq: 1 FOO-BAR");
    refused("CSeq method case", &raw);
    let raw = invite("INVITE", "invite sip:bob@example.com SIP/2.0", "");
    refused("CSeq INVITE for the extension method invite", &raw);
}

/// The request-line method is a token of its own (§25.1 `Method`), whatever
/// its CSeq says.
#[test]
fn a_request_line_method_is_a_token() {
    let raw = invite("INVITE", "FO<O sip:bob@example.com SIP/2.0", "")
        .replace("CSeq: 1 INVITE", "CSeq: 1 FO<O");
    let err = parse(&raw).expect_err("refused");
    assert!(err.contains("Method"), "refused by the request line: {err}");
}

/// An identity header the grammar refuses is a non-fatal `Err`: the message
/// parses, and the entry is not used.
#[test]
fn an_identity_header_the_grammar_refuses_is_stored_as_an_error() {
    let raw = invite(
        "",
        "",
        "Refer-To: sip:carol@192.0.2.9:5060?User-to-User=0430%3Bencoding%3Dhex\r\n\
         History-Info: <sip:bob@example.com?Reason=SIP;cause=302>;index=1.1\r\n\
         Diversion: <sip: +33612345678@example.com>;reason=unconditional\r\n",
    );
    let r = request(&raw);
    assert!(r.optional().refer_to.is_err(), "bare addr-spec with ?headers");
    assert!(r.optional().history_info.is_err(), "raw `;` in an escaped header");
    assert!(r.optional().diversion.is_err(), "a space inside <...>");
}

/// A final of 300 or more establishes no dialog (§12.1.1): a Record-Route on
/// it is not read, like its Contact, and the response is delivered.
#[test]
fn a_record_route_on_a_failure_final_does_not_refuse_it() {
    let raw = "SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-uri1\r\n\
               From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>;tag=b1\r\n\
               Call-ID: c1\r\nCSeq: 1 INVITE\r\nRecord-Route: sip:p.example.com;lr\r\n\
               Content-Length: 0\r\n\r\n";
    match parse(raw) {
        Ok(SipMessage::Response(r)) => assert_eq!(r.status(), 486),
        other => panic!("the 486 is delivered: {other:?}"),
    }
    refused("a 200 is held to it", &raw.replace("486 Busy Here", "200 OK"));
}

#[test]
fn an_ipv6_reference_holds_an_ipv6_address_and_nothing_else() {
    for bad in [
        "sip:a@[zz]",
        "sip:a@[1.2.3.4]",
        "sip:a@[::1%eth0]",
        "sip:a@[:::::]",
        "sip:a@[1:2:3:4g::1]",
        "sip:a@[::1]junk",
    ] {
        assert!(uri(bad).is_err(), "IPv6 {bad:?} accepted");
        refused("Contact IPv6", &invite("Contact", &format!("Contact: <{bad}>"), ""));
    }
    for good in ["sip:a@[2001:db8::1]:5070", "sip:a@[::ffff:192.0.2.1]", "sip:[::1]", "sip:[1::]"] {
        assert!(uri(good).is_ok(), "IPv6 {good:?} refused: {:?}", uri(good));
    }
}

#[test]
fn a_port_is_digits_in_range() {
    for bad in ["sip:a@example.com:", "sip:a@example.com:abc", "sip:a@example.com:5060abc"] {
        assert!(uri(bad).is_err(), "port {bad:?} accepted");
    }
    assert!(uri("sip:a@example.com:0").is_err(), "port 0 (ADR-0001)");
    assert_eq!(uri("sip:a@example.com:65535").map(|u| u.port()), Ok(Some(65535)));
}

#[test]
fn uri_parameters_are_paramchar_runs() {
    for bad in [
        "sip:a@example.com;x=\"q\"",
        "sip:a@example.com;=v",
        "sip:a@example.com;maddr=",
        "sip:a@example.com;x=a,b",
        "sip:a@example.com;x=a=b",
        "sip:a@example.com;x=%zz",
    ] {
        assert!(uri(bad).is_err(), "uri-parameter {bad:?} accepted");
        refused("From uri-parameter", &invite("From", &format!("From: <{bad}>;tag=a1"), ""));
    }
    let u = uri("sip:a@example.com;transport=tcp;lr;x-[a]=/:&+$;ttl=256").expect("paramchars");
    assert_eq!(u.param("ttl").and_then(|v| v.as_str()), Some("256"), "no value check on ttl");
}

#[test]
fn escaped_headers_are_name_value_pairs() {
    for bad in [
        "sip:a@example.com?a",
        "sip:a@example.com?=x",
        "sip:a@example.com?a=b&",
        "sip:a@example.com?a=b c",
        "sip:a@example.com?a=<b>",
    ] {
        assert!(uri(bad).is_err(), "?headers {bad:?} accepted");
    }
    let u = uri("sip:a@example.com?Replaces=x%40y%3Bto-tag%3D1&Subject=").expect("headers");
    assert_eq!(u.escaped_header("Replaces").as_deref(), Some("x@y;to-tag=1"));
}

#[test]
fn a_scheme_is_alpha_then_scheme_characters() {
    for bad in ["1x:example.com", ":example.com", "s p:example.com"] {
        assert!(uri(bad).is_err(), "scheme {bad:?} accepted");
        refused("Route scheme", &invite("", "", &format!("Route: <{bad}>\r\n")));
    }
}

#[test]
fn tel_and_other_schemes_stay_lenient_but_ascii() {
    for good in [
        "tel:+1-555-0100;phone-context=example.com",
        "tel:(555)0100",
        "tel:*31#;phone-context=+33",
        "urn:service:sos",
        "im:a@b",
    ] {
        assert!(uri(good).is_ok(), "{good:?} refused: {:?}", uri(good));
    }
    for bad in
        ["tel:+1555\u{eb}", "urn:service:s\u{f6}s", "tel:+1 555", "tel:+1555>x", "sip:*31#@h"]
    {
        assert!(uri(bad).is_err(), "{bad:?} accepted");
    }
    request(&invite("INVITE", "INVITE tel:*31#;phone-context=+33 SIP/2.0", ""));
}

// ---------------------------------------------------------------------------
// name-addr
// ---------------------------------------------------------------------------

#[test]
fn a_quoted_display_name_needs_angle_brackets() {
    refused(
        "From quoted display name, bare URI",
        &invite("From", "From: \"A\" sip:alice@example.com;tag=a1", ""),
    );
    assert!(NameAddr::parse(&SipStr::owned("\"A\" sip:alice@example.com")).is_err());
}

#[test]
fn nothing_but_parameters_follows_the_closing_bracket() {
    for from in [
        "From: <sip:alice@example.com>junk;tag=a1",
        "From: <sip:alice@example.com> <sip:bob@example.com>;tag=a1",
        "From: <sip:alice@example.com>;tag=a1 junk",
        "From: <sip:alice@example.com>;tag=a1;x=\"open",
    ] {
        refused("junk after >", &invite("From", from, ""));
    }
    let r = request(&invite("From", "From: <sip:alice@example.com> ; tag = a1 ;x=\"q;v\"", ""));
    assert_eq!(r.from().tag(), Some("a1"));
}

#[test]
fn a_bare_addr_spec_carries_no_comma_or_question_mark() {
    refused("bare From with ?", &invite("From", "From: sip:alice@example.com?x=y;tag=a1", ""));
    refused("bare To with ,", &invite("To", "To: sip:b,ob@example.com", ""));
    let r = request(&invite("From", "From: sip:alice@example.com;tag=a1", ""));
    assert_eq!(r.from().tag(), Some("a1"));
    assert!(r.from().uri().params().is_empty(), "a bare addr-spec's parameters are the header's");
}

#[test]
fn an_unquoted_display_name_is_tokens() {
    for from in [
        "From: Bell, Alexander <sip:a@example.com>;tag=a1",
        "From: A@B <sip:a@example.com>;tag=a1",
        "From: Zo\u{eb} <sip:a@example.com>;tag=a1",
    ] {
        refused("non-token display name", &invite("From", from, ""));
    }
    let r = request(&invite("From", "From: J Rosenberg\t<sip:a@example.com>;tag=a1", ""));
    assert_eq!(r.from().display(), Some("J Rosenberg"));
}

#[test]
fn a_contact_opened_with_a_bracket_closes_it() {
    refused("unclosed Contact", &invite("Contact", "Contact: <sip:alice@192.0.2.1;expires=60", ""));
}

#[test]
fn whitespace_inside_the_brackets_is_refused() {
    refused("LWS in <>", &invite("To", "To: < sip:bob@example.com>", ""));
    refused("LWS in <>", &invite("To", "To: <sip:bob@example.com >", ""));
}

// ---------------------------------------------------------------------------
// Route and Record-Route: eager, name-addr only
// ---------------------------------------------------------------------------

#[test]
fn a_route_is_a_name_addr() {
    refused("bare Route", &invite("", "", "Route: sip:p.example.com;lr\r\n"));
    refused("bare Record-Route", &invite("", "", "Record-Route: sip:p.example.com;lr\r\n"));
}

#[test]
fn a_route_uri_the_grammar_refuses_refuses_the_message() {
    for route in [
        "<sip:;lr>",
        "<sip:[::1]junk;lr>",
        "<sip:p.example.com junk;lr>",
        "<sip:p.example.com:5060-;lr>",
        "<sip:p.example.com:;lr>",
        "<sip:p.example.com;lr>junk",
    ] {
        refused("Route", &invite("", "", &format!("Route: {route}\r\n")));
        refused("Record-Route", &invite("", "", &format!("Record-Route: {route}\r\n")));
    }
}

#[test]
fn a_well_formed_route_set_reads_loose() {
    let r = request(&invite(
        "",
        "",
        "Route: <sip:p1.example.com;lr>, <sip:[2001:db8::7]:5070;lr>\r\n\
         Record-Route: <sip:p1.example.com;lr;ftag=a1>\r\n",
    ));
    let routes = r.route_set().expect("route set");
    assert_eq!(routes.iter().count(), 2);
    assert!(routes.iter().all(|e| e.uri().is_loose_route()));
    assert_eq!(r.record_route_set().expect("record-route set").iter().count(), 1);
}

// ---------------------------------------------------------------------------
// The character classes beyond the URI
// ---------------------------------------------------------------------------

#[test]
fn a_call_id_is_words() {
    for call_id in ["Call-ID: zo\u{eb}@example.com", "Call-ID: a b@example.com", "Call-ID: a@b@c"] {
        refused("Call-ID", &invite("Call-ID", call_id, ""));
    }
    let r = request(&invite("Call-ID", "Call-ID: a7-{x}/(y)<z>:\\\"[w]?@[2001:db8::1]", ""));
    assert_eq!(r.call_id().as_str(), "a7-{x}/(y)<z>:\\\"[w]?@[2001:db8::1]");
}

#[test]
fn a_tag_is_a_token() {
    refused("From tag", &invite("From", "From: <sip:alice@example.com>;tag=zo\u{eb}", ""));
    refused("From tag", &invite("From", "From: <sip:alice@example.com>;tag=a/b", ""));
    refused("To tag", &invite("To", "To: <sip:bob@example.com>;tag=\"q\"", ""));
    let r = request(&invite("From", "From: <sip:alice@example.com>;tag=a-b.c!%*_+`'~", ""));
    assert_eq!(r.from().tag(), Some("a-b.c!%*_+`'~"));
}

#[test]
fn a_branch_is_a_token() {
    refused(
        "branch",
        &invite("Via", "Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-zo\u{eb}", ""),
    );
}

#[test]
fn a_cseq_method_is_a_token() {
    let raw = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-uri1\r\n\
               From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>;tag=b1\r\n\
               Call-ID: c1\r\nCSeq: 1 INVIT\u{cb}\r\nContent-Length: 0\r\n\r\n";
    refused("CSeq method", raw);
}

#[test]
fn a_header_name_is_a_token() {
    refused("header name", &invite("", "", "X-Zo\u{eb}: 1\r\n"));
    refused("header name", &invite("", "", "X(Y): 1\r\n"));
}

// ---------------------------------------------------------------------------
// The Date: the one header the parser reads that never refuses the message
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_date_never_refuses_the_message() {
    request(&invite("", "", "Date: Sat, 13 Nov 2010 23:29:00 PST\r\n"));
    request(&invite("", "", "Date: Sat, 3 Nov 2010 23:29:00 GMT\r\n"));
    request(&invite("", "", "Date: Sat, 13 Nov 2010 23:29:00 GMT\r\n"));
}
