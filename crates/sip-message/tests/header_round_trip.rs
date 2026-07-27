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

// --- the anti-loss half of the property ---
//
// The fixpoint above is blind to loss that happens identically in both parse
// passes: a value whose parameter is dropped renders short, re-parses to the
// same short value, and the fixpoint holds. On the inputs a render is expected
// to reproduce octet for octet, the rendered LENGTH is therefore asserted
// against the trimmed input's, so a dropped parameter, a truncated host or a
// lost URI header fails the lane.

/// Assert `render(parse(input))` is exactly as long as `input` for every input
/// the `byte_preserving` predicate admits, and that enough inputs were admitted
/// for the assertion to mean something.
fn assert_no_loss<H: HeaderValue>(
    label: &str,
    inputs: &[String],
    byte_preserving: fn(&str) -> bool,
    floor: usize,
) {
    let mut checked = 0usize;
    for input in inputs {
        let trimmed = input.trim();
        if !byte_preserving(trimmed) {
            continue;
        }
        let Ok(values) = H::parse_line(&SipStr::owned(trimmed)) else { continue };
        let [value] = &values[..] else { continue };
        let rendered = value.to_wire();
        assert_eq!(
            rendered.len(),
            trimmed.len(),
            "{label}: render is not byte-preserving on an input that should be\n  \
             input    {trimmed:?}\n  rendered {rendered:?}"
        );
        checked += 1;
    }
    assert!(
        checked >= floor,
        "{label}: only {checked} of {} inputs were byte-preserving — the pin would be vacuous",
        inputs.len()
    );
}

/// A name-addr the renderer reproduces exactly: already bracketed (an
/// unbracketed addr-spec gains `<>` and a bare display name gains quotes), one
/// value per line, and no whitespace or quoted-string for the renderer to
/// re-lay-out.
fn addr_renders_verbatim(input: &str) -> bool {
    input.starts_with('<')
        && !input.contains(',')
        && !input.contains('"')
        && !input.bytes().any(|b| b == b' ' || b == b'\t')
}

/// A Via the renderer reproduces exactly: one value per line, and no
/// quoted-pair — RFC 3261 §25.1 lets a quoted string escape any character, and
/// re-rendering emits the escape only where the grammar requires one, which is
/// a normalization rather than a loss.
fn via_renders_verbatim(input: &str) -> bool {
    !input.contains(',') && !input.contains('\\')
}

/// A sequence-number header the renderer reproduces exactly: no redundant
/// leading zero, which renders as the number it means. Method case folds
/// without changing length.
fn sequence_renders_verbatim(input: &str) -> bool {
    input.split_whitespace().all(|t| !(t.len() > 1 && t.starts_with('0')))
}

/// A URI [`Uri::normalized`] reproduces octet for octet. The one exclusion is a
/// redundant leading zero in the port, which renders as the port it means —
/// a normalization, not a loss.
fn uri_renders_verbatim(input: &str) -> bool {
    let authority = input.split_once('?').map_or(input, |(a, _)| a);
    let after_scheme = authority.split_once(':').map_or("", |(_, rest)| rest);
    let host = after_scheme.rsplit('@').next().unwrap_or("").split(';').next().unwrap_or("");
    match host.rsplit_once(':') {
        Some((_, port)) => !(port.len() > 1 && port.starts_with('0')),
        None => true,
    }
}

// --- name-addr family, driven by the ABNF corpus ---

#[test]
fn from_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::From>("From", &corpus("from"), 900);
    assert_no_loss::<header::From>("From", &corpus("from"), addr_renders_verbatim, 400);
}

#[test]
fn contact_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::Contact>("Contact", &corpus("contact"), 900);
    assert_no_loss::<header::Contact>("Contact", &corpus("contact"), addr_renders_verbatim, 80);
}

#[test]
fn p_asserted_identity_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::PAssertedIdentity>("P-Asserted-Identity", &corpus("pai"), 900);
    assert_no_loss::<header::PAssertedIdentity>(
        "P-Asserted-Identity",
        &corpus("pai"),
        addr_renders_verbatim,
        100,
    );
}

#[test]
fn refer_to_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::ReferTo>("Refer-To", &corpus("refer-to"), 900);
    assert_no_loss::<header::ReferTo>("Refer-To", &corpus("refer-to"), addr_renders_verbatim, 350);
}

#[test]
fn route_values_are_a_render_parse_fixpoint() {
    // Route entries share the name-addr grammar; the Contact corpus is the
    // richest name-addr generator we have.
    assert_fixpoint::<header::RouteEntry>("Route", &corpus("contact"), 900);
    assert_no_loss::<header::RouteEntry>("Route", &corpus("contact"), addr_renders_verbatim, 80);
}

// --- Via, CSeq, RAck ---

#[test]
fn via_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<Via>("Via", &corpus("via"), 900);
    assert_no_loss::<Via>("Via", &corpus("via"), via_renders_verbatim, 450);
}

#[test]
fn cseq_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::CSeq>("CSeq", &corpus("cseq"), 900);
    assert_no_loss::<header::CSeq>("CSeq", &corpus("cseq"), sequence_renders_verbatim, 800);
}

#[test]
fn rack_values_are_a_render_parse_fixpoint() {
    assert_fixpoint::<header::RAck>("RAck", &corpus("rack"), 900);
    assert_no_loss::<header::RAck>("RAck", &corpus("rack"), sequence_renders_verbatim, 800);
}

// --- URIs, which every address value embeds ---

/// One URI through the fixpoint, plus the anti-loss length assertion wherever
/// the render is expected to be byte-preserving. `None` when the parser refused
/// the input, otherwise whether it was counted as byte-preserving.
fn uri_round_trip(input: &str) -> Option<bool> {
    let input = input.trim();
    let uri = Uri::parse(&SipStr::owned(input)).ok()?;
    // An unedited URI goes back out as the bytes it came in as, so the
    // fixpoint is driven through the field renderer — which is what an
    // edited URI is written with.
    assert_eq!(uri.to_string(), input, "URI: an unedited value was rewritten");
    let rendered = uri.clone().normalized().to_string();
    let reparsed = Uri::parse(&SipStr::owned(&rendered))
        .unwrap_or_else(|e| panic!("URI: rendered {rendered:?} no longer parses ({})", e.reason));
    assert_eq!(reparsed, uri, "URI: parse(render(v)) != v\n  input {input:?}");
    // The fixpoint alone cannot see a part of the URI that both passes drop;
    // the field renderer's output must also be as long as what it was read
    // from.
    if !uri_renders_verbatim(input) {
        return Some(false);
    }
    assert_eq!(
        rendered.len(),
        input.len(),
        "URI: the field renderer lost bytes\n  input    {input:?}\n  rendered {rendered:?}"
    );
    Some(true)
}

#[test]
fn uris_are_a_render_parse_fixpoint() {
    let inputs = corpus("sip-uri");
    let mut accepted = 0usize;
    let mut verbatim = 0usize;
    for input in &inputs {
        let Some(byte_preserving) = uri_round_trip(input) else { continue };
        verbatim += usize::from(byte_preserving);
        accepted += 1;
    }
    assert!(accepted >= 900, "URI: only {accepted} of {} inputs parsed", inputs.len());
    assert!(verbatim >= 800, "URI: only {verbatim} inputs were byte-preserving");
}

// The ABNF generator writes every escaped-header pair with its `=`, so the pair
// a peer may write without one — malformed under RFC 3261 §19.1.1, and passed
// through rather than deleted — is pinned by hand.
#[test]
fn an_escaped_header_pair_with_no_value_is_byte_preserving() {
    for input in ["sip:a@h?X-Trace", "sip:a@h?a=b&X-Trace&c=d", "sip:a@h;lr?X-Trace&Replaces=1"] {
        assert_eq!(uri_round_trip(input), Some(true), "URI: {input:?} lost its valueless pair");
    }
}

// An IPv6 host written without its brackets is refused, so no reader is handed
// the text before its first colon as a host.
#[test]
fn an_unbracketed_ipv6_host_is_refused_by_the_parser() {
    for input in ["sip:2001:db8::1", "sip:alice@2001:db8::1;transport=udp"] {
        assert_eq!(uri_round_trip(input), None, "URI: {input:?} was accepted");
    }
    assert_eq!(uri_round_trip("sip:bob@[2001:db8::1]:5060"), Some(true));
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
    // A value outside the kind's range is rejected, so each target drives the
    // inputs its own registry entry admits.
    let hops = owned(&["0", "1", "70", " 12 ", "255", "256", "seventy", "", "99999999999"]);
    assert_fixpoint::<NumericHeader<header::kind::MaxForwards>>("Max-Forwards", &hops, 5);
    let seconds = owned(&["0", "1", "3600", " 12 ", "4294967295", "seventy", "", "99999999999"]);
    assert_fixpoint::<NumericHeader<header::kind::Expires>>("Expires", &seconds, 5);
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
