//! Optional structured headers — the eager, non-fatal accessor model. Port of
//! `tests/sip/lazy-headers.test.ts`.
//!
//! ADR-0003 makes them **eager + non-fatal** fields on `OptionalHeaders` (parsed
//! once at parse time, a malformed value captured as `Err` rather than rejecting
//! the message): `msg.optional().p_asserted_identity`. Repeated access returns
//! the same stored field. The Geolocation family is read on access instead,
//! through the typed read surface.

use sip_message::header::{
    Geolocation, GeolocationError, GeolocationRouting, HeaderValue, NameAddr, RAck,
};
use sip_message::method::Method;
use sip_message::{CustomParser, SipMessage, SipParser};

const BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: lazy-test\r\n\
CSeq: 1 INVITE\r\n";

const PRACK_BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: lazy-test\r\n\
CSeq: 2 PRACK\r\n";

const REFER_BASE: &str = "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-test\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: lazy-test\r\n\
CSeq: 3 REFER\r\n";

fn build(start: &str, base: &str, optional: &str) -> SipMessage {
    let raw =
        format!("{start} sip:bob@example.com SIP/2.0\r\n{base}{optional}Content-Length: 0\r\n\r\n");
    CustomParser::new().parse(raw.as_bytes()).expect("parse should succeed")
}

fn invite(optional: &str) -> SipMessage {
    build("INVITE", BASE, optional)
}

fn na_param<'a>(na: &'a NameAddr, key: &str) -> Option<&'a str> {
    na.params().value(key)
}

// --- P-Asserted-Identity (RFC 3325) ---

#[test]
fn pai_absent_is_empty_list() {
    let msg = invite("");
    assert_eq!(msg.optional().p_asserted_identity.as_ref().unwrap(), &Vec::<NameAddr>::new());
}

#[test]
fn pai_single_value() {
    let msg = invite("P-Asserted-Identity: \"Cullen Jennings\" <sip:fluffy@cisco.com>\r\n");
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].display(), Some("Cullen Jennings"));
    assert_eq!(list[0].uri().text(), "sip:fluffy@cisco.com");
}

#[test]
fn pai_two_values_comma_separated() {
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>, <tel:+14085551212>\r\n");
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].uri().text(), "sip:fluffy@cisco.com");
    assert_eq!(list[1].uri().text(), "tel:+14085551212");
}

#[test]
fn pai_two_header_instances_combine() {
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>\r\nP-Asserted-Identity: <tel:+14085551212>\r\n");
    assert_eq!(msg.optional().p_asserted_identity.as_ref().unwrap().len(), 2);
}

#[test]
fn pai_field_is_stored_once_eagerly() {
    // The TS "memoization" test: in the eager model the parsed value is stored
    // on the message, so two accesses yield the same reference.
    let msg = invite("P-Asserted-Identity: <sip:fluffy@cisco.com>\r\n");
    let a = &msg.optional().p_asserted_identity;
    let b = &msg.optional().p_asserted_identity;
    assert!(std::ptr::eq(a, b));
}

#[test]
fn pai_comma_splitter_ignores_quoted_commas() {
    let msg = invite(
        "P-Asserted-Identity: \"Smith, John\" <sip:js@example.com>, <sip:jane@example.com>\r\n",
    );
    let list = msg.optional().p_asserted_identity.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].display(), Some("Smith, John"));
    assert_eq!(list[0].uri().text(), "sip:js@example.com");
    assert_eq!(list[1].uri().text(), "sip:jane@example.com");
}

// --- Diversion (RFC 5806) ---

#[test]
fn diversion_entry_params() {
    let msg =
        invite("Diversion: <sip:divert@example.com>;reason=user-busy;counter=2;privacy=full\r\n");
    let list = msg.optional().diversion.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uri().text(), "sip:divert@example.com");
    assert_eq!(na_param(&list[0], "reason"), Some("user-busy"));
    assert_eq!(na_param(&list[0], "counter"), Some("2"));
    assert_eq!(na_param(&list[0], "privacy"), Some("full"));
}

// --- History-Info (RFC 7044) ---

#[test]
fn history_info_indexed_entries() {
    let msg = invite("History-Info: <sip:alice@example.com>;index=1\r\nHistory-Info: <sip:redirect@example.com>;index=1.1\r\n");
    let list = msg.optional().history_info.as_ref().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(na_param(&list[0], "index"), Some("1"));
    assert_eq!(na_param(&list[1], "index"), Some("1.1"));
}

// --- Geolocation / -Routing / -Error (RFC 6442), read on access ---

#[test]
fn geolocation_list() {
    let msg = invite(
        "Geolocation: <https://ls.example.com/loc1>, <cid:loc2@example.com>;inserted-by=192.0.2.1\r\n",
    );
    let list = msg.list::<Geolocation>().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].uri().text(), "https://ls.example.com/loc1");
    assert_eq!(list[1].uri().text(), "cid:loc2@example.com");
    assert_eq!(list[1].params().value("inserted-by"), Some("192.0.2.1"));
}

/// `locationValue = LAQUOT locationURI RAQUOT *(SEMI geoloc-param)`: no
/// display name, no bare URI.
#[test]
fn geolocation_outside_its_grammar_is_err_on_read() {
    assert!(invite("Geolocation: cid:loc1@example.com\r\n").list::<Geolocation>().is_err());
    assert!(invite("Geolocation: \"Here\" <cid:loc1@example.com>\r\n")
        .list::<Geolocation>()
        .is_err());
}

/// §4.2: "yes" allows routing on the location; "no", or any value this
/// element does not understand, does not.
#[test]
fn geolocation_routing_yes_no_absent_unknown() {
    let routing =
        |line: &str| invite(line).header::<GeolocationRouting>().map(|r| r.unwrap().allowed());
    assert_eq!(routing("Geolocation-Routing: yes\r\n"), Some(true));
    assert_eq!(routing("Geolocation-Routing: NO\r\n"), Some(false));
    assert_eq!(routing("Geolocation-Routing: maybe\r\n"), Some(false));
    assert_eq!(routing(""), None);
}

/// §4.3: `locationErrorValue = location-error-code *(SEMI
/// location-error-params)`, the code three digits at most.
#[test]
fn geolocation_error_is_a_code_and_params() {
    let msg = invite("Geolocation-Error: 100;code=\"Cannot Process Location\"\r\n");
    let error = msg.header::<GeolocationError>().unwrap().unwrap();
    assert_eq!(error.code(), 100);
    assert_eq!(error.text(), Some("Cannot Process Location"));
    assert_eq!(error.to_wire(), "100;code=\"Cannot Process Location\"");
    let spaced = invite("Geolocation-Error: 200 ; code=\"None\"\r\n");
    assert_eq!(spaced.header::<GeolocationError>().unwrap().unwrap().code(), 200);
    for refused in ["1000", "busy", "<sip:locinfo@example.com>;code=200", "100 trailing"] {
        let msg = invite(&format!("Geolocation-Error: {refused}\r\n"));
        assert!(msg.header::<GeolocationError>().unwrap().is_err(), "{refused:?}");
    }
}

/// The parser reads none of the three: a malformed value costs nothing at
/// parse and never surfaces until a reader asks for it.
#[test]
fn geolocation_headers_are_not_read_at_parse() {
    let msg = invite(
        "Geolocation: junk\r\nGeolocation-Error: junk\r\nGeolocation-Routing: \"quoted\"\r\n",
    );
    assert!(!format!("{:?}", msg.optional()).contains("junk"));
    assert!(msg.list::<Geolocation>().is_err());
    assert!(msg.header::<GeolocationError>().unwrap().is_err());
}

// --- RAck (RFC 3262) ---

#[test]
fn rack_absent_is_none() {
    assert_eq!(*invite("").optional().rack.as_ref().unwrap(), None);
}

#[test]
fn rack_well_formed() {
    let msg = build("PRACK", PRACK_BASE, "RAck: 776656 1 INVITE\r\n");
    let rack = msg.optional().rack.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(rack, &RAck::new(776656, 1, Method::Invite));
}

#[test]
fn rack_extra_whitespace_tolerated() {
    let msg = build("PRACK", PRACK_BASE, "RAck:    42   7   INVITE\r\n");
    let rack = msg.optional().rack.as_ref().unwrap().clone().unwrap();
    assert_eq!(rack.rseq(), 42);
    assert_eq!(rack.seq(), 7);
    assert_eq!(rack.method(), &Method::Invite);
}

#[test]
fn rack_missing_method_or_non_numeric_is_err() {
    assert!(build("PRACK", PRACK_BASE, "RAck: 1 2\r\n").optional().rack.is_err());
    assert!(build("PRACK", PRACK_BASE, "RAck: foo 2 INVITE\r\n").optional().rack.is_err());
}

/// RFC 3262 §7.2: LWS separates the CSeq-num from the Method, and the Method
/// is a token. A malformed RAck stays a non-fatal `Err`.
#[test]
fn rack_without_lws_or_with_a_non_token_method_is_err() {
    assert!(build("PRACK", PRACK_BASE, "RAck: 776656 1INVITE\r\n").optional().rack.is_err());
    assert!(build("PRACK", PRACK_BASE, "RAck: 776656 1 INVITE/extra\r\n").optional().rack.is_err());
}

// --- Refer-To (RFC 3515 + RFC 3891) ---

fn refer(optional: &str) -> SipMessage {
    build("REFER", REFER_BASE, optional)
}

#[test]
fn refer_to_absent_is_none() {
    assert_eq!(*invite("").optional().refer_to.as_ref().unwrap(), None);
}

#[test]
fn refer_to_blind_transfer() {
    let msg = refer("Refer-To: <sip:carol@example.com>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.uri().text(), "sip:carol@example.com");
    assert!(r.uri().escaped_header("Replaces").is_none());
    assert_eq!(r.uri().escaped_headers().count(), 0);
    assert_eq!(r.uri().user(), Some("carol"));
    assert_eq!(r.uri().host(), "example.com");
}

#[test]
fn refer_to_attended_with_replaces() {
    let msg = refer(
        "Refer-To: <sip:carol@example.com?Replaces=abc-call-id%3Bto-tag%3Dt1%3Bfrom-tag%3Dt2>\r\n",
    );
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.uri().user(), Some("carol"));
    // The embedded Replaces is percent-decoded on read; its own grammar is the
    // dialog matcher's concern, not the header model's.
    assert_eq!(
        r.uri().escaped_header("Replaces").as_deref(),
        Some("abc-call-id;to-tag=t1;from-tag=t2")
    );
}

#[test]
fn refer_to_early_only() {
    let msg = refer("Refer-To: <sip:carol@example.com?Replaces=cid%3Bto-tag%3Dx%3Bfrom-tag%3Dy%3Bearly-only>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(
        r.uri().escaped_header("Replaces").as_deref(),
        Some("cid;to-tag=x;from-tag=y;early-only")
    );
}

#[test]
fn refer_to_display_name_literal_replaces_not_matched() {
    let msg = refer("Refer-To: \"Replaces?replaces=foo\" <sip:carol@example.com>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.display(), Some("Replaces?replaces=foo"));
    assert!(r.uri().escaped_header("Replaces").is_none());
    assert_eq!(r.uri().escaped_headers().count(), 0);
}

#[test]
fn refer_to_replaces_missing_tags_keeps_raw_but_no_struct() {
    let msg = refer("Refer-To: <sip:carol@example.com?Replaces=just-callid>\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.uri().escaped_header("Replaces").as_deref(), Some("just-callid"));
}

#[test]
fn refer_to_header_level_params() {
    let msg = refer("Refer-To: <sip:carol@example.com>;method=INVITE\r\n");
    let r = msg.optional().refer_to.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(r.params().value("method"), Some("INVITE"));
    assert!(r.uri().escaped_header("Replaces").is_none());
}

// --- Remote-Party-ID, P-Preferred-Identity ---

#[test]
fn remote_party_id_params() {
    let msg = invite("Remote-Party-ID: \"Alice\" <sip:alice@example.com>;party=calling;id-type=subscriber;privacy=off\r\n");
    let list = msg.optional().remote_party_id.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(na_param(&list[0], "party"), Some("calling"));
    assert_eq!(na_param(&list[0], "id-type"), Some("subscriber"));
    assert_eq!(na_param(&list[0], "privacy"), Some("off"));
}

#[test]
fn p_preferred_identity_present() {
    let msg = invite("P-Preferred-Identity: <sip:alice@example.com>\r\n");
    let list = msg.optional().p_preferred_identity.as_ref().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].uri().text(), "sip:alice@example.com");
}
