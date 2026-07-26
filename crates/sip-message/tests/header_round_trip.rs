//! ADR-0025 alignment pin: `parse(render(v)) == v` for every header value.
//!
//! Parse and render sit on one type, so the only way they can drift is for a
//! rendered value to read back as something else. This suite drives the frozen
//! ABNF corpus (`tests/abnf/corpus/`) — adversarial, grammar-generated input —
//! through that fixpoint, plus a hand-written corpus for the families the
//! generator has no grammar for.
//!
//! Inputs the parser rejects are skipped: the property is about the *image* of
//! parse. Each target asserts a floor on how many inputs it accepted, so a
//! parser that started rejecting everything could not pass vacuously.

use std::fs;
use std::path::PathBuf;

use sip_message::header::{
    self, Credentials, HeaderValue, NameAddrHeader, NumericHeader, TokenListHeader,
    TokenParamsHeader, Uri, Via,
};
use sip_message::SipStr;

fn corpus(target: &str) -> Vec<String> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/abnf/corpus");
    p.push(format!("{target}.txt"));
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read corpus {p:?}: {e}"));
    text.lines().filter(|l| !l.trim().is_empty()).map(str::to_owned).collect()
}

/// Drive every input through `parse`, re-`render` each accepted value and
/// re-`parse` it; the two values must be identical. Returns how many inputs
/// were accepted.
fn fixpoint<H: HeaderValue + PartialEq>(label: &str, inputs: &[String]) -> usize {
    let mut accepted = 0usize;
    for input in inputs {
        let raw = SipStr::owned(input);
        let Ok(values) = H::parse_line(&raw) else { continue };
        for value in &values {
            let rendered = value.to_wire();
            let reparsed = H::parse(&SipStr::owned(&rendered)).unwrap_or_else(|e| {
                panic!("{label}: rendered {rendered:?} no longer parses ({})\n  from {input:?}", e.reason)
            });
            assert_eq!(
                &reparsed, value,
                "{label}: parse(render(v)) != v\n  input    {input:?}\n  rendered {rendered:?}"
            );
            accepted += 1;
        }
    }
    accepted
}

fn assert_fixpoint<H: HeaderValue + PartialEq>(label: &str, inputs: &[String], floor: usize) {
    let accepted = fixpoint::<H>(label, inputs);
    assert!(
        accepted >= floor,
        "{label}: only {accepted} of {} inputs parsed — the pin would be vacuous",
        inputs.len()
    );
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| (*s).to_owned()).collect()
}

// --- name-addr family, driven by the ABNF corpus ---

#[test]
fn from_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::From>("From", &corpus("from"), 900);
}

#[test]
fn contact_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::Contact>("Contact", &corpus("contact"), 900);
}

#[test]
fn p_asserted_identity_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::PAssertedIdentity>("P-Asserted-Identity", &corpus("pai"), 900);
}

#[test]
fn refer_to_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::ReferTo>("Refer-To", &corpus("refer-to"), 900);
}

#[test]
fn route_values_are_a_render_parse_fixpoint() {
    // Route entries share the name-addr grammar; the Contact corpus is the
    // richest name-addr generator we have.
    assert_fixpoint::<header::RouteEntry>("Route", &corpus("contact"), 900);
}

// --- Via, CSeq, RAck ---

#[test]
fn via_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<Via>("Via", &corpus("via"), 900);
}

#[test]
fn cseq_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::CSeq>("CSeq", &corpus("cseq"), 900);
}

#[test]
fn rack_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::RAck>("RAck", &corpus("rack"), 900);
}

// --- URIs, which every address value embeds ---

#[test]
fn uris_are_a_render_parse_fixpoint() {
    let inputs = corpus("sip-uri");
    let mut accepted = 0usize;
    for input in &inputs {
        let Ok(uri) = Uri::parse(&SipStr::owned(input)) else { continue };
        let rendered = uri.to_string();
        let reparsed = Uri::parse(&SipStr::owned(&rendered))
            .unwrap_or_else(|e| panic!("URI: rendered {rendered:?} no longer parses ({})", e.reason));
        assert_eq!(reparsed, uri, "URI: parse(render(v)) != v\n  input {input:?}");
        accepted += 1;
    }
    assert!(accepted >= 900, "URI: only {accepted} of {} inputs parsed", inputs.len());
}

// --- the families the ABNF generator has no grammar for ---

#[test]
fn option_tag_sets_are_a_render_parse_fixpoint() {
    let inputs = owned(&[
        "100rel",
        "100rel, timer",
        " replaces ,  norefersub , gruu ",
        "100rel,100rel",
        "",
        "path, outbound, ice",
    ]);
    assert_fixpoint::<TokenListHeader<header::kind::Supported>>("Supported", &inputs, 5);
}

#[test]
fn token_parameter_headers_are_a_render_parse_fixpoint() {
    let inputs = owned(&[
        "application/sdp",
        "multipart/mixed;boundary=unique-boundary-1",
        "presence;id=4567",
        "terminated;reason=noresource",
        "active;expires=600;retry-after=0",
        r#"SIP;cause=486;text="Busy Here""#,
    ]);
    assert_fixpoint::<TokenParamsHeader<header::kind::ContentType>>("Content-Type", &inputs, 6);
    assert_fixpoint::<TokenParamsHeader<header::kind::SubscriptionState>>(
        "Subscription-State",
        &inputs,
        6,
    );
}

#[test]
fn numeric_headers_are_a_render_parse_fixpoint() {
    let inputs = owned(&["0", "1", "70", " 12 ", "4294967295", "seventy", "", "99999999999"]);
    assert_fixpoint::<NumericHeader<header::kind::MaxForwards>>("Max-Forwards", &inputs, 5);
}

#[test]
fn credentials_are_a_render_parse_fixpoint() {
    let inputs = owned(&[
        r#"Digest realm="atlanta.com", nonce="84a4cc6f", algorithm=MD5"#,
        r#"Digest username="bob", realm="biloxi.com", uri="sip:bob@biloxi.com", response="a", qop=auth, nc=00000001, cnonce="0a4f113b""#,
        "Digest",
        r#"Basic realm="a,b""#,
    ]);
    assert_fixpoint::<Credentials<header::kind::WwwAuthenticate>>("WWW-Authenticate", &inputs, 4);
}

// --- the kind axis is a compile-time fact, not a runtime check ---

#[test]
fn the_shared_name_addr_api_is_generic_over_the_kind() {
    fn identity<K: header::kind::NameAddrKind>(h: &NameAddrHeader<K>) -> &Uri {
        h.uri()
    }
    let from = header::From::parse(&SipStr::owned("<sip:alice@atlanta.com>;tag=1")).unwrap();
    let contact = header::Contact::parse(&SipStr::owned("<sip:alice@1.2.3.4:5060>")).unwrap();
    assert_eq!(identity(&from).host(), "atlanta.com");
    assert_eq!(identity(&contact).host_port(), ("1.2.3.4", 5060));
}
