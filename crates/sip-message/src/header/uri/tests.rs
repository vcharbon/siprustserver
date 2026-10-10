use super::*;

fn uri(s: &str) -> Uri {
    Uri::parse(&SipStr::owned(s)).unwrap_or_else(|e| panic!("{s}: {}", e.reason))
}

#[test]
fn a_full_uri_reads_every_part() {
    let u = uri("sips:alice@atlanta.com:5061;transport=tls;lr?Replaces=abc");
    assert_eq!(u.scheme(), "sips");
    assert!(u.is_secure());
    assert_eq!(u.user(), Some("alice"));
    assert_eq!(u.host_port(), ("atlanta.com", 5061));
    assert_eq!(u.param("transport").and_then(ParamValue::as_str), Some("tls"));
    assert!(u.is_loose_route());
    assert_eq!(u.escaped_header("replaces").as_deref(), Some("abc"));
}

// A URI headed for a Request-URI sheds its `?headers` list (RFC 3261
// §19.1.1) and keeps everything a Request-URI may carry.
#[test]
fn dropping_escaped_headers_keeps_the_rest_of_the_uri() {
    let bare = uri("sip:carol@chicago.com:5062;transport=tcp?Replaces=abc%3Bto-tag%3Dx")
        .without_escaped_headers();
    assert_eq!(bare.to_string(), "sip:carol@chicago.com:5062;transport=tcp");
    assert_eq!(bare.escaped_header("Replaces"), None);
}

#[test]
fn a_portless_uri_reports_the_default() {
    assert_eq!(uri("sip:bob@biloxi.com").host_port(), ("biloxi.com", 5060));
    assert_eq!(uri("sip:bob@biloxi.com").port(), None);
}

#[test]
fn an_ipv6_literal_loses_and_regains_its_brackets() {
    let u = uri("sip:[2001:db8::1]:5080");
    assert_eq!(u.host(), "2001:db8::1");
    assert_eq!(u.port(), Some(5080));
    assert_eq!(u.to_string(), "sip:[2001:db8::1]:5080");
    let with_user = uri("sip:bob@[2001:db8::1]:5060;transport=tcp");
    assert_eq!(with_user.host_port(), ("2001:db8::1", 5060));
    let text = "sip:bob@[2001:db8::1]:5060;transport=tcp";
    assert_eq!(with_user.normalized().to_string(), text);
    assert_eq!(uri("sip:[2001:db8::1]").host_port(), ("2001:db8::1", 5060));
}

// RFC 3261 §19.1.1 requires the brackets, so the value is malformed —
// refusing it keeps a router from resolving `2001`.
#[test]
fn an_unbracketed_ipv6_host_is_refused_not_truncated() {
    for text in ["sip:2001:db8::1", "sip:alice@2001:db8::1", "sip:2001:db8::1;transport=udp"] {
        assert!(Uri::parse(&SipStr::owned(text)).is_err(), "{text} was accepted");
        let opaque = Uri::parse_or_verbatim(&SipStr::owned(text));
        assert!(opaque.is_opaque());
        assert_eq!(opaque.to_string(), text);
    }
    // One colon is a port, not a truncation.
    assert_eq!(uri("sip:h:5061").host_port(), ("h", 5061));
}

#[test]
fn an_out_of_range_port_is_rejected() {
    assert!(Uri::parse(&SipStr::owned("sip:h:88161")).is_err());
}

#[test]
fn a_schemeless_value_is_opaque_not_a_uri() {
    assert!(Uri::parse(&SipStr::owned("*")).is_err());
    assert_eq!(Uri::parse_or_verbatim(&SipStr::owned("*")).to_string(), "*");
}

/// A port colon is not a scheme colon: a composer that reads one as the
/// other emits a name-addr holding no URI at all.
#[test]
fn a_port_colon_does_not_make_a_value_scheme_bearing() {
    assert!(Uri::value_has_scheme("sip:anonymous@host"));
    assert!(Uri::value_has_scheme("SIPS:bob@biloxi.com"));
    assert!(Uri::value_has_scheme("tel:+15550900002"));
    assert!(Uri::value_has_scheme("urn:service:sos"));
    assert!(!Uri::value_has_scheme("anonymous@198.51.100.20:5060"));
    assert!(!Uri::value_has_scheme("172.31.16.99"));
    assert!(!Uri::value_has_scheme("680181015550900002"));
    assert!(!Uri::value_has_scheme(""));
    assert!(!Uri::value_has_scheme(":5060"));
}

#[test]
fn an_unedited_uri_renders_the_bytes_it_was_read_from() {
    // Including the parts the field renderer would normalize away: the
    // scheme case and the case of a parameter name.
    let text = "SIP:Bob@biloxi.com:5060;Transport=TCP?Subject=hi&Replaces=abc";
    assert_eq!(uri(text).to_string(), text);
    assert_eq!(uri(text).scheme(), "sip");
}

#[test]
fn an_escaped_header_with_no_value_survives_a_render() {
    let one = uri("sip:a@h?X-Trace").normalized();
    assert_eq!(one.to_string(), "sip:a@h?X-Trace");
    assert_eq!(one.escaped_headers().collect::<Vec<_>>(), vec![("X-Trace", None)]);
    assert_eq!(one.escaped_header("X-Trace"), None);

    let mixed = uri("sip:a@h?a=b&X-Trace&c=d").normalized();
    assert_eq!(mixed.to_string(), "sip:a@h?a=b&X-Trace&c=d");
    assert_eq!(
        mixed.escaped_headers().collect::<Vec<_>>(),
        vec![("a", Some("b")), ("X-Trace", None), ("c", Some("d"))],
    );
    // A name with an empty value is not the same value as a name without one.
    assert_eq!(uri("sip:a@h?X-Trace=").normalized().to_string(), "sip:a@h?X-Trace=");
    assert_ne!(uri("sip:a@h?X-Trace="), uri("sip:a@h?X-Trace"));
}

#[test]
fn an_edited_uri_renders_from_its_parts() {
    let edited = uri("SIP:bob@biloxi.com").with_port(5070);
    assert_eq!(edited.to_string(), "sip:bob@biloxi.com:5070");
    assert_eq!(uri("SIP:bob@biloxi.com").normalized().to_string(), "sip:bob@biloxi.com");
}

#[test]
fn spelling_is_not_identity() {
    assert_eq!(uri("SIP:bob@biloxi.com"), uri("sip:bob@biloxi.com"));
    assert_eq!(uri("sip:bob@biloxi.com"), Uri::sip_user("bob", "biloxi.com"));
}

#[test]
fn text_borrows_the_source_and_renders_an_edited_uri() {
    let text = "SIP:Bob@biloxi.com;Transport=TCP";
    assert!(matches!(uri(text).text(), Cow::Borrowed(t) if t == text));
    assert_eq!(uri(text).with_port(5070).text(), "sip:Bob@biloxi.com:5070;Transport=TCP");
}

// Same subscriber, every URI byte different: scheme, host, port, params and
// RFC 3966 visual separators all drop out of the identity.
#[test]
fn user_identity_is_scheme_host_and_separator_insensitive() {
    assert!(uri("tel:+1-408-555-1212").same_user_identity(&uri("sip:+14085551212@gw.com")));
    assert!(uri("TEL:+1555").same_user_identity(&uri("tel:+1555")));
    assert_eq!(uri("tel:(408)555.1212").user_identity().as_deref(), Some("4085551212"));
    assert_eq!(
        uri("sip:+15550900012;verstat=TN-Validation-Passed@foo.example.com:5060;user=phone")
            .user_identity()
            .as_deref(),
        Some("+15550900012"),
    );
}

// A non-phone user is compared byte-exact (RFC 3261 §19.1.4), and a URI
// naming no user names no identity — so two userless URIs never match.
#[test]
fn user_identity_is_case_sensitive_and_absent_without_a_user() {
    assert_eq!(uri("sip:a.smith@example.com").user_identity().as_deref(), Some("a.smith"));
    assert!(!uri("sip:Alice@a.example").same_user_identity(&uri("sip:alice@b.example")));
    assert_eq!(uri("sip:10.0.0.1:5060").user_identity(), None);
    assert!(!uri("sip:host-only.example").same_user_identity(&uri("sip:host-only.example")));
}

// The dial form of one subscriber, however the peer spelled it, reduces to
// one digit string; a user-parameter never leaks into it.
#[test]
fn user_digits_drop_punctuation_and_one_leading_trunk_prefix() {
    assert_eq!(uri("tel:+1-555-0123").user_digits().as_deref(), Some("15550123"));
    assert_eq!(uri("sip:0015550123@h").user_digits().as_deref(), Some("15550123"));
    assert_eq!(uri("sip:015550123@h").user_digits().as_deref(), Some("15550123"));
    assert_eq!(uri("sip:15550123@h;npdi").user_digits().as_deref(), Some("15550123"));
    assert_eq!(
        uri("sip:+15550900012;verstat=TN-Validation-Passed@foo.example.com")
            .user_digits()
            .as_deref(),
        Some("15550900012"),
    );
    // Exactly ONE prefix goes: a number that really starts 0 keeps the rest.
    assert_eq!(uri("sip:0009001@h").user_digits().as_deref(), Some("09001"));
}

#[test]
fn user_digits_are_absent_where_the_identity_has_no_digit() {
    assert_eq!(uri("sip:anonymous@anonymous.invalid").user_digits(), None);
    assert_eq!(uri("sip:10.0.0.1:5060").user_digits(), None);
}

#[test]
fn functional_updates_compose() {
    let u = Uri::sip_user("bob", "biloxi.com").with_port(5070).with_flag("lr");
    assert_eq!(u.to_string(), "sip:bob@biloxi.com:5070;lr");
    assert_eq!(u.without_param("lr").without_user().to_string(), "sip:biloxi.com:5070");
}
