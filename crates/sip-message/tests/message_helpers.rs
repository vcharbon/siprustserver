//! Smoke tests for the pure header accessors / readers ported from
//! `MessageHelpers.ts`. The TS corpus has no dedicated MessageHelpers unit
//! test (only `MessageHelpers-random.test.ts`, which exercises the seeded-RNG
//! identifier generators — deferred to slice 2); these lock the behaviour the
//! generators port depends on.

use sip_message::message_helpers::{
    extract_contact_uri, extract_name_addr_uri, extract_tag, get_header, get_headers,
    header_param_value, parse_sip_uri, parse_via_params, remove_header, same_user_identity,
    set_header, strip_tag, uri_user_identity,
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

// --- header_param_value (P-Charging-Vector icid-value readout) --------------

const ICID: &str = "8agh3007ghb23oo5h9cc3ns6hpj4l0p96qsq28l5kqn651d8g6-5";

#[test]
fn header_param_value_reads_icid_from_leading_position() {
    // PCV's FIRST item is already a name=value pair (no leading `;`).
    let pcv = format!("icid-value={ICID};icid-generated-at=10.15.252.63");
    assert_eq!(header_param_value(&pcv, "icid-value").as_deref(), Some(ICID));
    assert_eq!(header_param_value(&pcv, "ICID-Value").as_deref(), Some(ICID), "name is case-insensitive");
}

#[test]
fn header_param_value_is_order_independent_and_ignores_mutating_siblings() {
    // The other PCV params mutate across an AS; only the named param matters.
    let a = format!("icid-value={ICID};icid-generated-at=10.15.252.63;orig-ioi=orange.fr");
    let b = format!("orig-ioi=btip.orange-business.com;term-ioi=x;icid-value={ICID}");
    assert_eq!(header_param_value(&a, "icid-value"), header_param_value(&b, "icid-value"));
    assert_eq!(header_param_value(&b, "icid-value").as_deref(), Some(ICID));
}

#[test]
fn header_param_value_unquotes_quoted_gen_value() {
    // gen-value = token / host / quoted-string (RFC 7315): quoted forms with
    // escapes (and an embedded `;`) resolve to the inner text.
    let pcv = r#"icid-value="quoted;icid\"x";orig-ioi=y"#;
    assert_eq!(header_param_value(pcv, "icid-value").as_deref(), Some(r#"quoted;icid"x"#));
}

#[test]
fn header_param_value_absent_vs_flag() {
    assert_eq!(header_param_value("icid-generated-at=10.0.0.1", "icid-value"), None);
    assert_eq!(header_param_value("lr;maddr=10.0.0.1", "lr").as_deref(), Some(""));
}

// --- uri_user_identity / same_user_identity ---------------------------------

#[test]
fn user_identity_matches_tel_and_sip_forms_across_hosts() {
    // The MOH01 shape: same subscriber, every URI byte-different.
    assert!(same_user_identity(
        "tel:+33772589500",
        "sip:+33772589500@ims.mnc001.mcc208.3gppnetwork.org;user=phone",
    ));
    assert!(same_user_identity("<tel:+33772589500>", "sip:+33772589500@anything:5070"));
}

#[test]
fn user_identity_drops_userinfo_params_and_uri_params() {
    // Real capture form: `verstat` rides the USER part, before the `@`.
    let a = "sip:+33969979518;verstat=TN-Validation-Passed@btip.orange-business.com:5060;user=phone";
    let b = "sip:+33969979518;verstat=No-TN-Validation@orange-multimedia.fr;user=phone";
    assert_eq!(uri_user_identity(a).as_deref(), Some("+33969979518"));
    assert!(same_user_identity(a, b));
}

#[test]
fn user_identity_normalizes_phone_visual_separators() {
    // RFC 3966 visual separators are not significant for phone identities,
    // and the scheme is case-normalized — TEL: ≡ tel: ≡ sip: forms.
    assert!(same_user_identity("tel:+1-408-555-1212", "sip:+14085551212@gw.example.com"));
    assert!(same_user_identity("TEL:+333", "tel:+333"));
    assert_eq!(uri_user_identity("tel:(408)555.1212").as_deref(), Some("4085551212"));
    // ...but a non-phone user keeps its literal form, compared byte-exact —
    // the user part is case-SENSITIVE (RFC 3261 §19.1.4).
    assert_eq!(uri_user_identity("sip:a.smith@example.com").as_deref(), Some("a.smith"));
    assert!(!same_user_identity("sip:Alice@a.example", "sip:alice@b.example"));
    assert!(same_user_identity("sip:alice@a.example", "sip:alice@b.example"));
}

#[test]
fn user_identity_negative_cases() {
    assert!(!same_user_identity("tel:+33772589500", "sip:+33772589501@host"));
    assert!(!same_user_identity("sip:host-only.example", "sip:host-only.example"), "userless never matches");
    assert_eq!(uri_user_identity("sip:10.0.0.1:5060"), None);
}
