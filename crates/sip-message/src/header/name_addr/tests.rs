use super::*;
use crate::header::{AddrGrammar, Reading, UriRole};

fn parse(s: &str) -> NameAddr {
    NameAddr::parse(&SipStr::owned(s)).unwrap_or_else(|e| panic!("{s}: {}", e.reason))
}

fn rendered(s: &str) -> String {
    let mut w = Wire::new();
    parse(s).render(&mut w);
    w.as_str().to_owned()
}

#[test]
fn a_quoted_display_name_is_unescaped_and_requoted() {
    let n = parse(r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#);
    assert_eq!(n.display(), Some(r#"Alice "A""#));
    assert_eq!(n.params().value("tag"), Some("1928"));
    assert_eq!(
        rendered(r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#),
        r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#
    );
}

/// The written form keeps the quotes and the escapes the value carried; an
/// unquoted name is its own bytes, and a bare addr-spec states none.
#[test]
fn the_display_name_as_written_keeps_its_quoting() {
    let written =
        |s: &str| NameAddr::display_as_written(&SipStr::owned(s)).map(|d| d.as_str().to_owned());
    assert_eq!(
        written(r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#).as_deref(),
        Some(r#""Alice \"A\"""#)
    );
    assert_eq!(written("Bob <sip:bob@biloxi.com>").as_deref(), Some("Bob"));
    assert_eq!(written("  Bob  <sip:bob@biloxi.com>").as_deref(), Some("Bob"));
    assert_eq!(written("<sip:bob@biloxi.com>"), None);
    assert_eq!(written("sip:bob@biloxi.com;tag=9"), None);
    assert_eq!(written(""), None);
}

#[test]
fn an_unquoted_display_name_is_kept() {
    assert_eq!(parse("Bob <sip:bob@biloxi.com>").display(), Some("Bob"));
}

/// `parse_identity` keeps a bare value's tail on the URI; bracketed text reads
/// as `parse` does.
#[test]
fn an_identity_reads_a_bare_tail_as_uri_parameters() {
    let bare = NameAddr::parse_identity(&SipStr::from_static("sip:a@h;user=phone;tag=x")).unwrap();
    assert_eq!(bare.uri().params().value("user"), Some("phone"));
    assert_eq!(bare.uri().params().value("tag"), Some("x"));
    assert!(bare.params().is_empty());
    let bracketed =
        NameAddr::parse_identity(&SipStr::from_static("\"B\" <sip:a@h;user=phone>;tag=x")).unwrap();
    assert_eq!(bracketed.display(), Some("B"));
    assert_eq!(bracketed.uri().params().value("user"), Some("phone"));
    assert_eq!(bracketed.params().value("tag"), Some("x"));
}

/// An untagged identity drops a stated tag, bare or bracketed, and refuses
/// one hidden in the userinfo or a URI header.
#[test]
fn an_untagged_identity_drops_its_tag_and_refuses_a_hidden_one() {
    let read = |raw: &'static str| NameAddr::parse_untagged_identity(&SipStr::from_static(raw));
    let bare = read("sip:a@h;user=phone;tag=x").unwrap();
    assert_eq!(bare.uri().params().value("tag"), None);
    assert_eq!(bare.uri().params().value("user"), Some("phone"));
    let bracketed = read("\"B\" <sip:a@h;user=phone>;tag=x").unwrap();
    assert_eq!(bracketed.params().value("tag"), None);
    assert_eq!(bracketed.display(), Some("B"));
    assert!(read("<sip:a;tag=x@h>").is_err());
    assert!(read("<sip:a@h?X=tag=x>").is_err());
    assert!(read("<sip:a@h").is_err());
}

#[test]
fn an_addr_spec_takes_its_parameters_as_header_parameters() {
    let n = parse("sip:alice@atlanta.com;tag=99");
    assert_eq!(n.uri().host(), "atlanta.com");
    assert!(n.uri().params().is_empty());
    assert_eq!(n.params().value("tag"), Some("99"));
    assert_eq!(rendered("sip:alice@atlanta.com;tag=99"), "<sip:alice@atlanta.com>;tag=99");
}

#[test]
fn a_bracketed_uri_keeps_its_own_parameters() {
    let n = parse("<sip:p.example;lr>;tag=7");
    assert!(n.uri().is_loose_route());
    assert_eq!(n.params().value("tag"), Some("7"));
}

/// A strict read consumes its whole value: text after the parameters, a
/// parameter written twice and a second address on the line are refused.
#[test]
fn a_value_with_anything_after_its_parameters_is_refused() {
    for raw in [
        "<sip:c@example.com> junk",
        "<sip:c@example.com>;x=1 junk",
        "<sip:c@example.com>;x=1;x=1",
        "<sip:c@example.com>, <sip:d@example.com>",
    ] {
        assert!(NameAddr::parse(&SipStr::owned(raw)).is_err(), "{raw}");
    }
}

/// A bare value whose `;` comes before its `@` reads up to that `@` only
/// where the grammar names the reading, after the strict read fails; a
/// valid bare value reads as it did.
#[test]
fn a_bare_user_semicolon_reads_where_its_grammar_names_it() {
    let grammar = AddrGrammar {
        bare_user_semicolon: true,
        ..AddrGrammar::of(AddrForm::NameAddrOrSpec, UriRole::Identity)
    };
    let read = |raw: &str| NameAddr::read(&SipStr::owned(raw), &grammar, Reading::Strict);
    let bare = read("sip:a;b=c@example.com;tag=a1").expect("the reading");
    assert!(bare.bare_user_semicolon);
    assert_eq!(bare.addr.uri().host(), "example.com");
    assert_eq!(bare.addr.params().value("tag"), Some("a1"));
    assert!(read("sip:a?b=c@example.com;tag=a1").is_err());
    let valid = read("sip:example.com;x=\"a@b\";tag=1").expect("valid");
    assert!(!valid.bare_user_semicolon);
    assert_eq!(valid.addr.params().value("x"), Some("a@b"));
    assert!(NameAddr::parse(&SipStr::owned("sip:a;b=c@example.com;tag=a1")).is_err());
}

#[test]
fn an_unclosed_angle_bracket_is_an_error() {
    assert!(NameAddr::parse(&SipStr::owned("<sip:a@h")).is_err());
}

// RFC 3261 §20: a URI holding a `;` is bracketed in a header; inside the
// brackets it reads whole, and a bare addr-spec ends at its first `;`.
#[test]
fn a_bracketed_address_keeps_a_semicolon_user_part_a_bare_one_does_not() {
    let target = "sip:+33612345678;foo=bar@192.0.2.10:5070";
    let bracketed = parse(&format!("<{target}>;expires=60"));
    assert_eq!(bracketed.uri().to_string(), target);
    assert_eq!(bracketed.uri().host_port(), ("192.0.2.10", 5070));
    let bare = NameAddr::parse(&SipStr::owned(target));
    assert!(bare.map_or(true, |addr| addr.uri().to_string() != target));
}

/// A target stated outside a header line reads whole when it is a URI and
/// as a name-addr otherwise.
#[test]
fn a_stated_target_reads_bare_or_bracketed() {
    let target = "sip:+33612345678;foo=bar@192.0.2.10:5070;user=phone";
    let read = |s: &str| NameAddr::parse_uri_or_name_addr(&SipStr::owned(s)).unwrap();
    let bare = read(target);
    assert_eq!(bare.uri().to_string(), target);
    assert!(bare.params().is_empty());
    let bracketed = read(&format!("\"Bob\" <{target}>;q=0.5"));
    assert_eq!(bracketed.uri().to_string(), target);
    assert_eq!(bracketed.display(), Some("Bob"));
    assert_eq!(bracketed.params().value("q"), Some("0.5"));
    assert!(NameAddr::parse_uri_or_name_addr(&SipStr::owned("<sip:a@h")).is_err());
}

/// RFC 3261 §20 brackets a URI holding `,`, `?` or `;` in Contact, From and To.
/// Elsewhere a bare addr-spec reads a `?` as the start of its URI's headers;
/// a `,` is its own only in a single-valued header (RFC 3515 §2.1, RFC 3892
/// §3), and a `;` after it starts the header parameters wherever it stands.
#[test]
fn a_bare_addr_spec_holds_what_its_header_leaves_unbracketed() {
    use crate::header::{Contact, Diversion, From, HeaderValue, ReferTo, ReferredBy, To};
    let bare = "sip:carol@192.0.2.9?Subject=hi%20there;x=1";
    let refer = ReferTo::parse(&SipStr::owned(bare)).expect("a bare Refer-To with headers");
    assert_eq!(refer.uri().escaped_header("Subject").as_deref(), Some("hi there"));
    assert_eq!(refer.params().value("x"), Some("1"), "a `;` after it is the header's");
    assert!(refer.uri().params().value("x").is_none());
    let comma = "sip:carol,ext@192.0.2.9";
    assert!(ReferTo::parse(&SipStr::owned(comma)).is_ok(), "single-valued: the `,` is the URI's");
    assert!(ReferredBy::parse(&SipStr::owned(bare)).is_ok());
    assert!(Diversion::parse(&SipStr::owned(bare)).is_ok(), "a list header reads the `?`");
    assert!(Diversion::parse(&SipStr::owned(comma)).is_err(), "a list header's `,` separates");
    for refused in [bare, comma] {
        let raw = SipStr::owned(refused);
        assert!(From::parse(&raw).is_err(), "From: {refused}");
        assert!(To::parse(&raw).is_err(), "To: {refused}");
        assert!(Contact::parse(&raw).is_err(), "Contact: {refused}");
    }
}
